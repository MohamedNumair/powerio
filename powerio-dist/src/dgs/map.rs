//! Layer 3: DGS objects into the multiconductor [`DistNetwork`].
//!
//! This is the reader half unique to the distribution target. Where the
//! balanced reader collapses DGS to positive sequence, this one keeps the
//! zero-sequence and per-phase data and reconstructs genuine 3x3 conductor
//! matrices (Fortescue), wye/delta transformer connections from the vector
//! group, per-phase loads, and one `VoltageSource` from the external grid.
//!
//! Units are SI throughout: volts, watts, vars, ohms, siemens, meters, and
//! radians. `base_frequency` is `ElmNet.frnom`. No system MVA base is needed;
//! the distribution model is dimensional.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::f64::consts::TAU;

use serde_json::Value;

use super::FMT;
use super::matrix::{MatrixRegistry, matrix_attr};
use super::object::{Topology, index_objects, resolve, topology};
use super::scan::{DgsDoc, DgsRow, DgsTable, cell, int, num, ptr, text};
use crate::error::{Error, Result};
use crate::model::{
    Configuration, DistBus, DistGenerator, DistLine, DistLineCode, DistLoad, DistLoadVoltageModel,
    DistNetwork, DistShunt, DistSwitch, DistTransformer, Extras, Mat, VoltageSource, Winding,
    WindingConn,
};

/// PowerFactory's default nominal frequency when no grid states one.
const DEFAULT_FRNOM: f64 = 50.0;

/// The materialized grounded neutral terminal. DGS distribution conductors are
/// phases 1..3, so the neutral is 4 on every wye/single-phase bus, matching
/// the number the dss reader and the public BMOPF/PMD examples give it.
const NEUTRAL: &str = "4";

/// Per-bus conductor state accumulated while elements are mapped.
struct BusAccum {
    id: String,
    /// Phase conductor indices seen on this bus (1/2/3).
    phases: BTreeSet<u8>,
    /// A wye, single-phase, or source element asked for a neutral return.
    needs_neutral: bool,
    uknom: f64,
}

/// The whole layer-3 reader: resolves references and builds the network.
pub(crate) struct Reader<'a> {
    doc: &'a DgsDoc,
    by_id: HashMap<&'a str, (usize, usize)>,
    matrices: MatrixRegistry,
    decimal: char,
    warnings: Vec<String>,
    /// Surviving bus (root terminal position) -> accumulated state. The
    /// BTreeMap key is the union-find root, i.e. the smallest DGS terminal
    /// position in the set, so iterating by key yields buses in DGS order.
    bus: BTreeMap<usize, BusAccum>,
    /// Names already handed out per namespace (buses, linecodes, elements).
    used_bus: BTreeSet<String>,
    used_linecode: BTreeSet<String>,
    used_elem: BTreeSet<String>,
    /// TypLne id -> linecode name, for the shared single-type case.
    linecode_cache: HashMap<String, String>,
    /// One-shot count of single-phase elements with no pinned phase.
    defaulted_phase: usize,
    /// Keys of the once-per-run structural warnings already emitted.
    warned_once: BTreeSet<&'static str>,
    /// Per-class out-of-service counts, surfaced as one warning each.
    skipped_outserv: BTreeMap<&'static str, usize>,
}

impl<'a> Reader<'a> {
    pub(crate) fn build(doc: &'a DgsDoc, name_hint: Option<&str>) -> Result<DistNetwork> {
        let by_id = index_objects(doc);
        let matrices = MatrixRegistry::build(doc);
        let reader = Reader {
            doc,
            by_id,
            matrices,
            decimal: doc.decimal,
            warnings: Vec::new(),
            bus: BTreeMap::new(),
            used_bus: BTreeSet::new(),
            used_linecode: BTreeSet::new(),
            used_elem: BTreeSet::new(),
            linecode_cache: HashMap::new(),
            defaulted_phase: 0,
            warned_once: BTreeSet::new(),
            skipped_outserv: BTreeMap::new(),
        };
        reader.run(name_hint)
    }

    #[allow(clippy::too_many_lines)]
    fn run(mut self, name_hint: Option<&str>) -> Result<DistNetwork> {
        if self.doc.table("ElmTerm").is_none_or(|t| t.rows.is_empty()) {
            return Err(Error::FormatRead {
                format: FMT,
                message: "this DGS file carries only operational updates (no ElmTerm topology); \
                          export the full model"
                    .into(),
            });
        }

        let (name, base_frequency) = self.grid_info(name_hint);
        let mut topo = topology(self.doc);

        // Fuse terminals joined by closed couplers; open couplers become
        // switches, collected after the union-find settles.
        let mut open_couplers: Vec<(usize, usize, Option<String>)> = Vec::new();
        let mut fused = 0usize;
        if let Some(coup) = self.doc.table("ElmCoup") {
            for r in &coup.rows {
                let id = coup.id_of(r);
                let Some((a, b, ca, cb)) = two_terminals(&topo, &id) else {
                    continue;
                };
                let on = int(coup, r, "on_off", 1, self.decimal)? != 0;
                let isclosed = int(coup, r, "isclosed", 1, self.decimal)? != 0;
                let outserv = int(coup, r, "outserv", 0, self.decimal)? != 0;
                if on && isclosed && !outserv && ca && cb {
                    topo.uf.union(a, b);
                    fused += 1;
                } else {
                    open_couplers.push((a, b, text(coup, r, "loc_name").map(str::to_owned)));
                }
            }
        }
        if fused > 0 {
            self.warnings.push(format!(
                "fused {fused} terminal pair(s) joined by closed couplers"
            ));
        }

        // Surviving bus per position; the union-find root (smallest position in
        // the fused set) keys the bus, so BTreeMap order is DGS order.
        let n = topo.terminals.len();
        topo.bus_of_pos = (0..n).map(|pos| topo.uf.find(pos)).collect();
        let term_table = self.doc.table("ElmTerm").expect("checked above");
        let terms = &topo.terminals;
        let decimal = self.decimal;
        for pos in 0..n {
            let root = topo.bus_of_pos[pos];
            let uknom = num(term_table, terms[pos], "uknom", 0.0, decimal)?;
            let acc = self.bus.entry(root).or_insert_with(|| {
                let raw = text(term_table, terms[root], "loc_name").unwrap_or("bus");
                BusAccum::seed(raw)
            });
            if acc.uknom.abs() < 1e-9 {
                acc.uknom = uknom;
            }
        }
        // Assign unique display ids in bus order.
        let roots: Vec<usize> = self.bus.keys().copied().collect();
        for root in roots {
            let raw = self.bus[&root].id.clone();
            let unique = self.unique_name(&raw, Namespace::Bus);
            self.bus.get_mut(&root).expect("root in map").id = unique;
        }

        let mut linecodes: Vec<DistLineCode> = Vec::new();
        let mut lines: Vec<DistLine> = Vec::new();
        let mut switches: Vec<DistSwitch> = Vec::new();
        let mut transformers: Vec<DistTransformer> = Vec::new();
        let mut loads: Vec<DistLoad> = Vec::new();
        let mut generators: Vec<DistGenerator> = Vec::new();
        let mut shunts: Vec<DistShunt> = Vec::new();
        let mut sources: Vec<VoltageSource> = Vec::new();

        self.lines(&topo, base_frequency, &mut linecodes, &mut lines)?;
        self.transformers_2w(&topo, &mut transformers)?;
        self.transformers_3w(&topo, &mut transformers)?;
        self.loads(&topo, &mut loads)?;
        self.machines(&topo, &mut generators)?;
        self.external_grids(&topo, &mut sources, &mut generators)?;
        self.shunts(&topo, &mut shunts)?;

        // Open couplers become open switches between the surviving buses.
        for (a, b, nm) in open_couplers {
            let (ra, rb) = (topo.bus_of_pos[a], topo.bus_of_pos[b]);
            if ra == rb {
                continue;
            }
            let map_a = phase_terminals(&self.bus_phase_vec(ra), false);
            let map_b = phase_terminals(&self.bus_phase_vec(rb), false);
            let (id_a, id_b) = (self.bus_id(ra), self.bus_id(rb));
            let sw_name = self.unique_name(nm.as_deref().unwrap_or("coup"), Namespace::Elem);
            switches.push(DistSwitch::new(sw_name, id_a, id_b, map_a, map_b, true));
        }

        // Finalize buses from the accumulators, in DGS (BTreeMap key) order.
        let mut buses = Vec::with_capacity(self.bus.len());
        for acc in self.bus.values() {
            let mut phases: Vec<u8> = acc.phases.iter().copied().collect();
            if phases.is_empty() {
                phases = vec![1, 2, 3]; // an unconnected terminal defaults to 3-phase
            }
            // A 4-wire explicit-matrix line already carries conductor 4 as a
            // real neutral wire; a wye/single-phase/source element only asks
            // for a grounded neutral. Materialize "4" once, and ground it when
            // some element grounds it.
            let has_neutral_wire = phases.contains(&4);
            let mut terminals: Vec<String> = phases.iter().map(u8::to_string).collect();
            let mut grounded = Vec::new();
            if acc.needs_neutral {
                if !has_neutral_wire {
                    terminals.push(NEUTRAL.to_string());
                }
                grounded.push(NEUTRAL.to_string());
            }
            let mut bus = DistBus::new(acc.id.clone(), terminals);
            bus.grounded = grounded;
            buses.push(bus);
        }

        // Warnings: defaulted phases, skipped out-of-service, missing source.
        if self.defaulted_phase > 0 {
            self.warnings.push(format!(
                "{} single-phase element(s) defaulted to phase 1; DGS did not pin a phase \
                 (StaCubic cPhInfo)",
                self.defaulted_phase
            ));
        }
        for (class, count) in std::mem::take(&mut self.skipped_outserv) {
            self.warnings.push(format!(
                "{count} out-of-service `{class}` element(s) skipped (the distribution model has \
                 no disable flag)"
            ));
        }
        if sources.is_empty() {
            self.warnings
                .push("no ElmXnet external grid; BMOPF requires exactly one voltage source".into());
        }
        self.warn_ignored_classes();

        let warnings = std::mem::take(&mut self.warnings);
        Ok(DistNetwork {
            name: Some(name),
            base_frequency,
            buses,
            linecodes,
            lines,
            switches,
            transformers,
            loads,
            generators,
            shunts,
            sources,
            warnings,
            // DGS is a read-only distribution source: leaving `source_format`
            // unset keeps it out of the byte-exact echo tier (no DGS writer
            // exists on the distribution model).
            source_format: None,
            ..DistNetwork::default()
        })
    }

    // ---- grid --------------------------------------------------------------

    /// The grid name and base frequency from the first `ElmNet`.
    fn grid_info(&mut self, name_hint: Option<&str>) -> (String, f64) {
        let mut name = None;
        let mut freq = None;
        let mut disagree = false;
        if let Some(net) = self.doc.table("ElmNet") {
            for r in &net.rows {
                let f = num(net, r, "frnom", DEFAULT_FRNOM, self.decimal).unwrap_or(DEFAULT_FRNOM);
                match freq {
                    None => {
                        freq = Some(f);
                        name = text(net, r, "loc_name").map(str::to_owned);
                    }
                    Some(first) if (first - f).abs() > 1e-6 => disagree = true,
                    _ => {}
                }
            }
        }
        if disagree {
            self.warnings
                .push("DGS grids declare different nominal frequencies; used the first".into());
        }
        (
            name.or_else(|| name_hint.map(str::to_owned))
                .unwrap_or_else(|| "case".into()),
            freq.unwrap_or(DEFAULT_FRNOM),
        )
    }

    // ---- lines -------------------------------------------------------------

    #[expect(clippy::too_many_lines)]
    fn lines(
        &mut self,
        topo: &Topology<'a>,
        f: f64,
        linecodes: &mut Vec<DistLineCode>,
        lines: &mut Vec<DistLine>,
    ) -> Result<()> {
        let Some(t) = self.doc.table("ElmLne") else {
            return Ok(());
        };
        // Group ElmLnesec by parent line (fold_id).
        let mut sections: HashMap<String, Vec<&DgsRow>> = HashMap::new();
        let sec_table = self.doc.table("ElmLnesec");
        if let Some(st) = sec_table {
            for r in &st.rows {
                if let Some(parent) = ptr(st, r, "fold_id") {
                    sections.entry(parent.to_string()).or_default().push(r);
                }
            }
        }

        for r in &t.rows {
            let id = t.id_of(r);
            let Some((from, to, cf, ct)) = two_terminals(topo, &id) else {
                continue;
            };
            if int(t, r, "outserv", 0, self.decimal)? != 0 || !cf || !ct {
                *self.skipped_outserv.entry("ElmLne").or_default() += 1;
                continue;
            }
            let nlnum = num(t, r, "nlnum", 1.0, self.decimal)?.max(1.0);
            let fline = num(t, r, "fline", 1.0, self.decimal)?;

            // Aggregate one section's totals (ohms/siemens) across sections, or
            // the direct type; the linecode is per-metre, the length in metres.
            let mut acc = LineAccum::default();
            let secs = sections.get(&id).filter(|s| !s.is_empty());
            let mut geometry = false;
            let mut sequence_type: Option<String> = None; // linecode cache key
            if let Some(secs) = secs {
                let st = sec_table.expect("sections came from ElmLnesec");
                let mut rows: Vec<&'a DgsRow> = secs.clone();
                rows.sort_by(|a, b| {
                    num(st, a, "index", 0.0, self.decimal)
                        .unwrap_or(0.0)
                        .partial_cmp(&num(st, b, "index", 0.0, self.decimal).unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                for sec in rows {
                    let len = num(st, sec, "dline", 0.0, self.decimal)? * 1000.0;
                    match self.line_imp(st, sec, &id)? {
                        LineImp::Geometry => {
                            geometry = true;
                            break;
                        }
                        imp => self.accumulate(imp, len, nlnum, f, &mut acc),
                    }
                }
            } else {
                let len = num(t, r, "dline", 1.0, self.decimal)? * 1000.0;
                match self.line_imp(t, r, &id)? {
                    LineImp::Geometry => geometry = true,
                    imp @ LineImp::Sequence { .. } => {
                        // A shared linecode only for the plain, underated type.
                        if nlnum <= 1.0 && (fline - 1.0).abs() < 1e-9 {
                            sequence_type = ptr(t, r, "typ_id").map(str::to_owned);
                        }
                        self.accumulate(imp, len, nlnum, f, &mut acc);
                    }
                    imp => self.accumulate(imp, len, nlnum, f, &mut acc),
                }
            }
            if geometry {
                self.warnings.push(format!(
                    "line {id:?} references a geometry/cable-system type with no exported line \
                     parameters (R_c1/X_c1/B_c1); re-export from PowerFactory with calculated line \
                     parameters, or use a lumped TypLne. Line skipped"
                ));
                continue;
            }

            let n_cond = acc.n_conductors.max(1);
            let length = if acc.length > 0.0 { acc.length } else { 1.0 };
            acc.length = length;

            let cached = sequence_type
                .as_ref()
                .and_then(|k| self.linecode_cache.get(k).cloned());
            let lc_name = if let Some(existing) = cached {
                existing
            } else {
                let base = sequence_type
                    .as_ref()
                    .and_then(|k| resolve(self.doc, &self.by_id, k))
                    .and_then(|(tt, tr)| text(tt, tr, "loc_name").map(str::to_owned))
                    .unwrap_or_else(|| format!("{id}_lc"));
                let unique = self.unique_name(&base, Namespace::Linecode);
                linecodes.push(acc.into_linecode(&unique, fline));
                if let Some(k) = &sequence_type {
                    self.linecode_cache.insert(k.clone(), unique.clone());
                }
                unique
            };

            let phase_ids: Vec<u8> = (1..=n_cond as u8).collect();
            let map_from = self.record_line(topo, from, &phase_ids);
            let map_to = self.record_line(topo, to, &phase_ids);
            let (bf, bt) = (
                self.bus_id(topo.bus_of_pos[from]),
                self.bus_id(topo.bus_of_pos[to]),
            );
            let name = self.unique_name(text(t, r, "loc_name").unwrap_or(&id), Namespace::Elem);
            let mut line = DistLine::new(name, bf, bt, map_from, map_to, lc_name, length);
            line.extras = extras(
                t,
                r,
                &["typ_id", "dline", "fline", "nlnum", "outserv", "pStoch"],
            );
            lines.push(line);
        }
        Ok(())
    }

    /// Classify a line/section holder into its impedance representation, in
    /// priority order: an explicit natural-coordinate matrix (verbatim, may be
    /// asymmetric or 4-wire), then symmetrical sequence data (Fortescue), then a
    /// geometry type with no exported parameters (skipped by the caller).
    fn line_imp(&self, t: &'a DgsTable, r: &'a DgsRow, line: &str) -> Result<LineImp> {
        let typ = ptr(t, r, "typ_id").unwrap_or("");
        let resolved = resolve(self.doc, &self.by_id, typ);

        // Tier 1: an explicit phase matrix on the type row or the line row.
        // PowSyBl's PowerFactory reader reads R_c1/X_c1/G_c1/B_c1 for tower
        // types; the rmatrix/xmatrix/bmatrix spellings are also accepted.
        //
        // TODO(dgs-cable-system): a `TypCabsys`/`ElmTow` may reference its
        // conductor matrices through a `pGeo`/`pStoch` chain of its own. When
        // those linkage attributes are known, follow them here before falling
        // through to the geometry tier.
        for (mt, mr) in [Some((t, r)), resolved].into_iter().flatten() {
            let r_mat = matrix_attr(&self.matrices, mt, mr, "R_c1", self.decimal)
                .or_else(|| matrix_attr(&self.matrices, mt, mr, "rmatrix", self.decimal));
            if let Some(r_mat) = r_mat {
                let n = r_mat.len();
                let zero = || vec![vec![0.0; n]; n];
                let x_mat = matrix_attr(&self.matrices, mt, mr, "X_c1", self.decimal)
                    .or_else(|| matrix_attr(&self.matrices, mt, mr, "xmatrix", self.decimal))
                    .unwrap_or_else(zero);
                let g_mat =
                    matrix_attr(&self.matrices, mt, mr, "G_c1", self.decimal).unwrap_or_else(zero);
                let b_mat = matrix_attr(&self.matrices, mt, mr, "B_c1", self.decimal)
                    .or_else(|| matrix_attr(&self.matrices, mt, mr, "bmatrix", self.decimal))
                    .unwrap_or_else(zero);
                return Ok(LineImp::Explicit {
                    r: r_mat,
                    x: x_mat,
                    g: g_mat,
                    b: b_mat,
                    n,
                });
            }
        }

        // Tiers 2 and 3 need a resolved type.
        let Some((tt, tr)) = resolved else {
            return Err(missing_type(line, typ));
        };
        if tt.class == "TypLne" {
            let d = self.decimal;
            let nph = int(tt, tr, "nlnph", 3, d)?.clamp(1, 3) as usize;
            let has_zero = cell(tt, tr, "rline0").is_some()
                || cell(tt, tr, "xline0").is_some()
                || cell(tt, tr, "cline0").is_some();
            return Ok(LineImp::Sequence {
                r1: num(tt, tr, "rline", 0.0, d)?,
                x1: num(tt, tr, "xline", 0.0, d)?,
                c1: num(tt, tr, "cline", 0.0, d)?,
                r0: num(tt, tr, "rline0", 0.0, d)?,
                x0: num(tt, tr, "xline0", 0.0, d)?,
                c0: num(tt, tr, "cline0", 0.0, d)?,
                sline: num(tt, tr, "sline", 0.0, d)?,
                nph,
                has_zero,
            });
        }
        if matches!(tt.class.as_str(), "TypTow" | "TypGeo" | "TypCabsys") {
            return Ok(LineImp::Geometry);
        }
        Err(missing_type(line, typ))
    }

    /// Accumulate one section's total ohms/siemens into the running aggregate.
    #[allow(clippy::many_single_char_names)]
    fn accumulate(&mut self, imp: LineImp, len_m: f64, nlnum: f64, f: f64, acc: &mut LineAccum) {
        match imp {
            LineImp::Explicit { r, x, g, b, n } => {
                if self.warned_once.insert("explicit") {
                    self.warnings.push(format!(
                        "explicit {n}-conductor phase matrix used verbatim (natural coordinates; \
                         asymmetry and any neutral conductor preserved)"
                    ));
                }
                // `*_c1` matrices are totals for the modelled length; add them
                // as this section's totals — parallel circuits divide the
                // series and multiply the shunt.
                acc.n_conductors = acc.n_conductors.max(n);
                acc.ensure(n.saturating_sub(1));
                for i in 0..n {
                    for j in 0..n {
                        acc.r[i][j] += r[i][j] / nlnum;
                        acc.x[i][j] += x[i][j] / nlnum;
                        acc.g_half[i][j] += g[i][j] * nlnum / 2.0;
                        acc.b_half[i][j] += b[i][j] * nlnum / 2.0;
                    }
                }
                acc.length += len_m;
            }
            LineImp::Sequence {
                r1,
                x1,
                c1,
                r0,
                x0,
                c0,
                sline,
                nph,
                has_zero,
            } => {
                if self.warned_once.insert("reconstructed") {
                    self.warnings.push(
                        "symmetrical TypLne expanded to a transposed 3x3; per-phase asymmetry is \
                         not represented by this type"
                            .into(),
                    );
                }
                if nph >= 2 && !has_zero && self.warned_once.insert("no_zero") {
                    self.warnings.push(
                        "one or more line types have no zero-sequence data; modelled uncoupled \
                         (Z0 = Z1)"
                            .into(),
                    );
                }
                if nph == 2 && self.warned_once.insert("two_phase") {
                    self.warnings.push(
                        "two-phase line sequence data is underdetermined; modelled with the \
                         positive-sequence self impedance and no mutual"
                            .into(),
                    );
                }
                let n = nph;
                acc.n_conductors = acc.n_conductors.max(n);
                acc.ensure(n.saturating_sub(1));
                let series = len_m / nlnum;
                let shunt = len_m * nlnum / 2.0; // half the total at each end
                // Per-metre positive/zero from Ohm/km and microfarad/km.
                let (rp1, xp1) = (r1 / 1000.0, x1 / 1000.0);
                let (rp0, xp0) = if has_zero {
                    (r0 / 1000.0, x0 / 1000.0)
                } else {
                    (rp1, xp1)
                };
                let cp1 = c1 * 1e-6 / 1000.0;
                let cp0 = if has_zero { c0 * 1e-6 / 1000.0 } else { cp1 };
                if n == 1 {
                    acc.r[0][0] += rp1 * series;
                    acc.x[0][0] += xp1 * series;
                    acc.b_half[0][0] += TAU * f * cp1 * shunt;
                } else {
                    let (rs, rm) = seq_pair(rp1, rp0);
                    let (xs, xm) = seq_pair(xp1, xp0);
                    let (bs, bm) = seq_pair(TAU * f * cp1, TAU * f * cp0);
                    // Two-phase sequence data is underdetermined: self only.
                    let (rm, xm, bm) = if n == 2 {
                        (0.0, 0.0, 0.0)
                    } else {
                        (rm, xm, bm)
                    };
                    let bs = if n == 2 { TAU * f * cp1 } else { bs };
                    for i in 0..n {
                        acc.r[i][i] += rs * series;
                        acc.x[i][i] += xs * series;
                        acc.b_half[i][i] += bs * shunt;
                        for j in 0..n {
                            if i != j {
                                acc.r[i][j] += rm * series;
                                acc.x[i][j] += xm * series;
                                acc.b_half[i][j] += bm * shunt;
                            }
                        }
                    }
                }
                acc.length += len_m;
                if sline > 0.0 {
                    acc.i_max = Some(sline * 1000.0);
                }
            }
            LineImp::Geometry => {}
        }
    }

    // ---- transformers ------------------------------------------------------

    fn transformers_2w(
        &mut self,
        topo: &Topology<'a>,
        transformers: &mut Vec<DistTransformer>,
    ) -> Result<()> {
        let Some(t) = self.doc.table("ElmTr2") else {
            return Ok(());
        };
        for r in &t.rows {
            let id = t.id_of(r);
            let Some((from, to, cf, ct)) = two_terminals(topo, &id) else {
                continue;
            };
            if int(t, r, "outserv", 0, self.decimal)? != 0 || !cf || !ct {
                *self.skipped_outserv.entry("ElmTr2").or_default() += 1;
                continue;
            }
            let typ = ptr(t, r, "typ_id").unwrap_or("");
            let (tt, tr) = resolve(self.doc, &self.by_id, typ)
                .filter(|(tt, _)| tt.class == "TypTr2")
                .ok_or_else(|| missing_type(&id, typ))?;
            let d = self.decimal;

            let strn = num(tt, tr, "strn", 0.0, d)?.max(1e-9);
            let ntnum = num(t, r, "ntnum", 1.0, d)?.max(1.0);
            let s_rating = strn * 1e6 * ntnum;
            let utrn_h = num(tt, tr, "utrn_h", 0.0, d)? * 1000.0;
            let utrn_l = num(tt, tr, "utrn_l", 0.0, d)? * 1000.0;

            // Short-circuit impedance uktr% splits into resistance uktrr% (or
            // pcutr kW) and reactance sqrt(z^2 - r^2), so xsc is reactance only
            // and each winding carries half the resistance.
            let uktr = num(tt, tr, "uktr", 0.0, d)?;
            let r_total = if cell(tt, tr, "uktrr").is_some() {
                num(tt, tr, "uktrr", 0.0, d)?
            } else {
                num(tt, tr, "pcutr", 0.0, d)? / (10.0 * strn)
            };
            let x_pct = (uktr * uktr - r_total * r_total).max(0.0).sqrt();
            let r_pct = r_total / 2.0;

            // Tap on the tap_side winding: 1 + (nntap - nntap0) * dutap/100.
            let nntap = num(t, r, "nntap", 0.0, d)?;
            let nntap0 = num(tt, tr, "nntap0", 0.0, d)?;
            let dutap = num(tt, tr, "dutap", 0.0, d)?;
            let tap_side = int(tt, tr, "tap_side", 0, d)?;
            let tap = 1.0 + (nntap - nntap0) * dutap / 100.0;

            let (conn_h, _gnd_h) = winding_conn(text(tt, tr, "tr2cn_h"));
            let (conn_l, _gnd_l) = winding_conn(text(tt, tr, "tr2cn_l"));
            let phases = int(t, r, "nphase", 3, d)?.clamp(1, 3) as usize;
            let ph: Vec<u8> = (1..=phases as u8).collect();

            let (bh, bl) = (
                self.bus_id(topo.bus_of_pos[from]),
                self.bus_id(topo.bus_of_pos[to]),
            );
            let map_h = self.record_winding(topo, from, &ph, conn_h);
            let map_l = self.record_winding(topo, to, &ph, conn_l);

            let mut w_h = Winding::new(bh, map_h, conn_h, utrn_h, s_rating);
            w_h.r_pct = r_pct;
            w_h.tap = if tap_side == 0 { tap } else { 1.0 };
            let mut w_l = Winding::new(bl, map_l, conn_l, utrn_l, s_rating);
            w_l.r_pct = r_pct;
            w_l.tap = if tap_side == 1 { tap } else { 1.0 };

            let name = self.unique_name(text(t, r, "loc_name").unwrap_or(&id), Namespace::Elem);
            let mut xf = DistTransformer::new(name, vec![w_h, w_l], vec![x_pct], phases);
            xf.extras = xf_extras(t, r, tt, tr);
            transformers.push(xf);
        }
        Ok(())
    }

    fn transformers_3w(
        &mut self,
        topo: &Topology<'a>,
        transformers: &mut Vec<DistTransformer>,
    ) -> Result<()> {
        let Some(t) = self.doc.table("ElmTr3") else {
            return Ok(());
        };
        for r in &t.rows {
            let id = t.id_of(r);
            let conns = topo.conns.get(&id);
            let side =
                |s: i64| conns.and_then(|c| c.iter().find(|c| c.side == s).map(|c| c.term_pos));
            let (Some(hv), Some(mv), Some(lv)) = (side(0), side(1), side(2)) else {
                continue;
            };
            if int(t, r, "outserv", 0, self.decimal)? != 0 {
                *self.skipped_outserv.entry("ElmTr3").or_default() += 1;
                continue;
            }
            let typ = ptr(t, r, "typ_id").unwrap_or("");
            let (tt, tr) = resolve(self.doc, &self.by_id, typ)
                .filter(|(tt, _)| tt.class == "TypTr3")
                .ok_or_else(|| missing_type(&id, typ))?;
            let d = self.decimal;
            let phases = 3usize;

            // Star short-circuit voltages uktr3_h/m/l give the pairwise
            // reactances; the resistive parts ur3tr_* peel off the magnitude.
            let x_pair = |z: &str, r: &str| -> Result<f64> {
                let z = num(tt, tr, z, 0.0, d)?;
                let r = num(tt, tr, r, 0.0, d)?;
                Ok((z * z - r * r).max(0.0).sqrt())
            };
            let xsc = vec![
                x_pair("uktr3_h", "ur3tr_h")?,
                x_pair("uktr3_l", "ur3tr_l")?,
                x_pair("uktr3_m", "ur3tr_m")?,
            ];

            let mut windings = Vec::with_capacity(3);
            for (pos, cn, uv, sv) in [
                (hv, "tr3cn_h", "utrn3_h", "strn3_h"),
                (mv, "tr3cn_m", "utrn3_m", "strn3_m"),
                (lv, "tr3cn_l", "utrn3_l", "strn3_l"),
            ] {
                let (conn, _gnd) = winding_conn(text(tt, tr, cn));
                let ph: Vec<u8> = (1..=phases as u8).collect();
                let map = self.record_winding(topo, pos, &ph, conn);
                let bus = self.bus_id(topo.bus_of_pos[pos]);
                windings.push(Winding::new(
                    bus,
                    map,
                    conn,
                    num(tt, tr, uv, 0.0, d)? * 1000.0,
                    num(tt, tr, sv, 0.0, d)? * 1e6,
                ));
            }

            let name = self.unique_name(text(t, r, "loc_name").unwrap_or(&id), Namespace::Elem);
            transformers.push(DistTransformer::new(name, windings, xsc, phases));
        }
        Ok(())
    }

    // ---- loads -------------------------------------------------------------

    fn loads(&mut self, topo: &Topology<'a>, loads: &mut Vec<DistLoad>) -> Result<()> {
        // ElmLod: general (default 3-phase) load with an optional TypLod model.
        if let Some(t) = self.doc.table("ElmLod") {
            for r in &t.rows {
                self.one_load(topo, t, r, false, loads)?;
            }
        }
        // ElmLodlv / ElmLodmv: default single-phase low/medium-voltage loads.
        for class in ["ElmLodlv", "ElmLodmv"] {
            if let Some(t) = self.doc.table(class) {
                for r in &t.rows {
                    self.one_load(topo, t, r, true, loads)?;
                }
            }
        }
        if let Some(t) = self.doc.table("ElmLodlvp") {
            if !t.rows.is_empty() {
                self.warnings.push(format!(
                    "`ElmLodlvp` table ignored ({} rows): partial line loads are not mapped",
                    t.rows.len()
                ));
            }
        }
        Ok(())
    }

    fn one_load(
        &mut self,
        topo: &Topology<'a>,
        t: &'a DgsTable,
        r: &'a DgsRow,
        lv_default_single: bool,
        loads: &mut Vec<DistLoad>,
    ) -> Result<()> {
        let id = t.id_of(r);
        let Some((pos, closed, pinned)) = one_terminal(topo, &id) else {
            return Ok(());
        };
        if int(t, r, "outserv", 0, self.decimal)? != 0 || !closed {
            *self.skipped_outserv.entry("ElmLod").or_default() += 1;
            return Ok(());
        }
        let d = self.decimal;
        let scale = num(t, r, "scale0", 1.0, d)?;
        let p_w = num(t, r, "plini", 0.0, d)? * scale * 1e6;
        let q_var = if cell(t, r, "qlini").is_some() {
            num(t, r, "qlini", 0.0, d)? * scale * 1e6
        } else {
            let s = num(t, r, "slini", 0.0, d)? * scale * 1e6;
            let kap = if int(t, r, "pf_recap", 0, d)? == 1 {
                -1.0
            } else {
                1.0
            };
            kap * (s * s - p_w * p_w).max(0.0).sqrt()
        };

        // Single-phase when the class defaults to it or phtech marks 1-phase.
        let phtech = int(t, r, "phtech", -1, d)?;
        let single = (lv_default_single && phtech < 0) || matches!(phtech, 2 | 3 | 8);

        let root = topo.bus_of_pos[pos];
        let bus = self.bus_id(root);
        let vll = self.bus_uknom(root) * 1000.0;
        let (config, terminal_map, p_nom, q_nom, v_phase, active) = if single {
            let phase = pinned.unwrap_or_else(|| {
                self.defaulted_phase += 1;
                1
            });
            let map = self.record_wye(topo, pos, &[phase]);
            (
                Configuration::SinglePhase,
                map,
                vec![p_w],
                vec![q_var],
                vll / 3f64.sqrt(),
                1usize,
            )
        } else {
            let ph = [1u8, 2, 3];
            let map = self.record_wye(topo, pos, &ph);
            (
                Configuration::Wye,
                map,
                vec![p_w / 3.0; 3],
                vec![q_var / 3.0; 3],
                vll / 3f64.sqrt(),
                3usize,
            )
        };

        let name = self.unique_name(text(t, r, "loc_name").unwrap_or(&id), Namespace::Elem);
        let mut load = DistLoad::new(name, bus, terminal_map, config, p_nom, q_nom);
        load.voltage_model = self.load_voltage_model(t, r, v_phase, active);
        load.extras = extras(
            t,
            r,
            &[
                "typ_id", "plini", "qlini", "slini", "scale0", "pf_recap", "phtech", "outserv",
                "ulini", "for_name",
            ],
        );
        loads.push(load);
        Ok(())
    }

    /// TypLod `kpu`/`kqu` become an exponential model; otherwise constant power.
    fn load_voltage_model(
        &self,
        t: &'a DgsTable,
        r: &'a DgsRow,
        v_phase: f64,
        active: usize,
    ) -> DistLoadVoltageModel {
        let v_nom = vec![v_phase; active];
        let Some(typ) = ptr(t, r, "typ_id") else {
            return DistLoadVoltageModel::ConstantPower { v_nom };
        };
        let Some((tt, tr)) =
            resolve(self.doc, &self.by_id, typ).filter(|(tt, _)| tt.class == "TypLod")
        else {
            return DistLoadVoltageModel::ConstantPower { v_nom };
        };
        let kpu = num(tt, tr, "kpu", 0.0, self.decimal).unwrap_or(0.0);
        let kqu = num(tt, tr, "kqu", 0.0, self.decimal).unwrap_or(0.0);
        if kpu.abs() < 1e-12 && kqu.abs() < 1e-12 {
            return DistLoadVoltageModel::ConstantPower { v_nom };
        }
        DistLoadVoltageModel::Exponential {
            v_nom,
            gamma_p: vec![kpu; active],
            gamma_q: vec![kqu; active],
        }
    }

    // ---- machines & external grids ----------------------------------------

    fn machines(&mut self, topo: &Topology<'a>, gens: &mut Vec<DistGenerator>) -> Result<()> {
        for class in ["ElmSym", "ElmGenstat", "ElmPvsys", "ElmAsm"] {
            let Some(t) = self.doc.table(class) else {
                continue;
            };
            for r in &t.rows {
                let id = t.id_of(r);
                let Some((pos, closed, _)) = one_terminal(topo, &id) else {
                    continue;
                };
                if int(t, r, "outserv", 0, self.decimal)? != 0 || !closed {
                    *self.skipped_outserv.entry("machine").or_default() += 1;
                    continue;
                }
                let d = self.decimal;
                let ngnum = num(t, r, "ngnum", 1.0, d)?.max(1.0);
                let scale = num(t, r, "scale0", 1.0, d)?;
                let p_w = num(t, r, "pgini", 0.0, d)? * ngnum * scale * 1e6;
                let q_var = num(t, r, "qgini", 0.0, d)? * ngnum * 1e6;
                let bus = self.bus_id(topo.bus_of_pos[pos]);
                let map = self.record_wye(topo, pos, &[1, 2, 3]);
                let name = self.unique_name(text(t, r, "loc_name").unwrap_or(&id), Namespace::Elem);
                let mut generator = DistGenerator::new(
                    name,
                    bus,
                    map,
                    Configuration::Wye,
                    vec![p_w / 3.0; 3],
                    vec![q_var / 3.0; 3],
                );
                if cell(t, r, "Pmax_uc").is_some() {
                    generator.p_max = Some(vec![num(t, r, "Pmax_uc", 0.0, d)? * 1e6 / 3.0; 3]);
                }
                if cell(t, r, "Pmin_uc").is_some() {
                    generator.p_min = Some(vec![num(t, r, "Pmin_uc", 0.0, d)? * 1e6 / 3.0; 3]);
                }
                if cell(t, r, "cQ_max").is_some() {
                    generator.q_max = Some(vec![num(t, r, "cQ_max", 0.0, d)? * 1e6 / 3.0; 3]);
                }
                if cell(t, r, "cQ_min").is_some() {
                    generator.q_min = Some(vec![num(t, r, "cQ_min", 0.0, d)? * 1e6 / 3.0; 3]);
                }
                generator.extras = extras(
                    t,
                    r,
                    &[
                        "typ_id", "pgini", "qgini", "ngnum", "scale0", "outserv", "Pmax_uc",
                        "Pmin_uc", "cQ_max", "cQ_min", "usetp", "ip_ctrl", "av_mode",
                    ],
                );
                gens.push(generator);
            }
        }
        Ok(())
    }

    fn external_grids(
        &mut self,
        topo: &Topology<'a>,
        sources: &mut Vec<VoltageSource>,
        gens: &mut Vec<DistGenerator>,
    ) -> Result<()> {
        let Some(t) = self.doc.table("ElmXnet") else {
            return Ok(());
        };
        for r in &t.rows {
            let id = t.id_of(r);
            let Some((pos, closed, _)) = one_terminal(topo, &id) else {
                continue;
            };
            if int(t, r, "outserv", 0, self.decimal)? != 0 || !closed {
                *self.skipped_outserv.entry("ElmXnet").or_default() += 1;
                continue;
            }
            let d = self.decimal;
            let root = topo.bus_of_pos[pos];
            let bus = self.bus_id(root);
            // uknom is line-to-line kV; the source magnitude is phase-to-neutral.
            let uknom = {
                let u = num(t, r, "uknom", 0.0, d)?;
                if u.abs() < 1e-9 {
                    self.bus_uknom(root)
                } else {
                    u
                }
            };
            let usetp = num(t, r, "usetp", 1.0, d)?;
            let phiini = num(t, r, "phiini", 0.0, d)?.to_radians();
            let vpn = uknom * 1000.0 / 3f64.sqrt() * usetp;
            let map = self.record_wye(topo, pos, &[1, 2, 3]);
            let name = self.unique_name(text(t, r, "loc_name").unwrap_or(&id), Namespace::Elem);

            if sources.is_empty() {
                let v_mag = vec![vpn, vpn, vpn, 0.0];
                let v_ang = vec![phiini, phiini - TAU / 3.0, phiini + TAU / 3.0, 0.0];
                let mut vs = VoltageSource::new(name, bus, map, v_mag, v_ang);
                vs.extras = extras(t, r, &["bustp", "uknom", "usetp", "phiini", "outserv"]);
                sources.push(vs);
            } else {
                // BMOPF allows one source; further grids become generators.
                self.warnings.push(
                    "more than one ElmXnet external grid; the first is the voltage source and the \
                     rest are modelled as generators"
                        .into(),
                );
                gens.push(DistGenerator::new(
                    name,
                    bus,
                    map,
                    Configuration::Wye,
                    vec![0.0; 3],
                    vec![0.0; 3],
                ));
            }
        }
        Ok(())
    }

    // ---- shunts ------------------------------------------------------------

    fn shunts(&mut self, topo: &Topology<'a>, shunts: &mut Vec<DistShunt>) -> Result<()> {
        let Some(t) = self.doc.table("ElmShnt") else {
            return Ok(());
        };
        for r in &t.rows {
            let id = t.id_of(r);
            let Some((pos, closed, _)) = one_terminal(topo, &id) else {
                continue;
            };
            if int(t, r, "outserv", 0, self.decimal)? != 0 || !closed {
                *self.skipped_outserv.entry("ElmShnt").or_default() += 1;
                continue;
            }
            let d = self.decimal;
            let root = topo.bus_of_pos[pos];
            let base_v = self.bus_uknom(root) * 1000.0;
            let shtype = int(t, r, "shtype", 2, d)?;
            let ncapa = num(t, r, "ncapa", 1.0, d)?.max(1.0);
            let qtotn = num(t, r, "qtotn", 0.0, d)? * 1e6; // Mvar rating -> var
            // Susceptance from the reactive rating at the phase-to-neutral base
            // voltage: b = Q / (3 * Vpn^2) per phase, capacitive positive.
            let vpn = base_v / 3f64.sqrt();
            let b_phase = if vpn.abs() < 1e-9 {
                0.0
            } else {
                let sign = if shtype == 1 { -1.0 } else { 1.0 };
                sign * ncapa * qtotn / (3.0 * vpn * vpn)
            };
            let map = self.record_wye(topo, pos, &[1, 2, 3]);
            let bus = self.bus_id(root);
            let name = self.unique_name(text(t, r, "loc_name").unwrap_or(&id), Namespace::Elem);
            let mut shunt =
                DistShunt::new(name, bus, map, diag_matrix(3, 0.0), diag_matrix(3, b_phase));
            shunt.extras = extras(t, r, &["shtype", "ncapa", "qtotn", "ushnm", "outserv"]);
            shunts.push(shunt);
        }
        Ok(())
    }

    // ---- terminal / bus helpers -------------------------------------------

    /// Record a line's phase conductors on both cubicle buses (no neutral).
    fn record_line(&mut self, topo: &Topology, pos: usize, phases: &[u8]) -> Vec<String> {
        self.touch(topo.bus_of_pos[pos], phases, false);
        phase_terminals(phases, false)
    }

    /// Record a winding: wye keeps the grounded neutral, delta does not.
    fn record_winding(
        &mut self,
        topo: &Topology,
        pos: usize,
        phases: &[u8],
        conn: WindingConn,
    ) -> Vec<String> {
        let wye = conn == WindingConn::Wye;
        self.touch(topo.bus_of_pos[pos], phases, wye);
        phase_terminals(phases, wye)
    }

    /// Record a wye connection (phases + grounded neutral).
    fn record_wye(&mut self, topo: &Topology, pos: usize, phases: &[u8]) -> Vec<String> {
        self.touch(topo.bus_of_pos[pos], phases, true);
        phase_terminals(phases, true)
    }

    fn touch(&mut self, root: usize, phases: &[u8], neutral: bool) {
        if let Some(acc) = self.bus.get_mut(&root) {
            for &p in phases {
                acc.phases.insert(p);
            }
            acc.needs_neutral = acc.needs_neutral || neutral;
        }
    }

    fn bus_id(&self, root: usize) -> String {
        self.bus
            .get(&root)
            .map(|b| b.id.clone())
            .unwrap_or_default()
    }

    fn bus_uknom(&self, root: usize) -> f64 {
        self.bus.get(&root).map_or(0.0, |b| b.uknom)
    }

    fn bus_phase_vec(&self, root: usize) -> Vec<u8> {
        self.bus.get(&root).map_or_else(
            || vec![1, 2, 3],
            |b| {
                if b.phases.is_empty() {
                    vec![1, 2, 3]
                } else {
                    b.phases.iter().copied().collect()
                }
            },
        )
    }

    /// A sanitized, unique name in the given namespace.
    fn unique_name(&mut self, raw: &str, ns: Namespace) -> String {
        let base = sanitize(raw);
        let base = if base.is_empty() {
            ns.fallback().to_string()
        } else {
            base
        };
        let used = match ns {
            Namespace::Bus => &mut self.used_bus,
            Namespace::Linecode => &mut self.used_linecode,
            Namespace::Elem => &mut self.used_elem,
        };
        if used.insert(base.clone()) {
            return base;
        }
        let mut n = 1u32;
        loop {
            let candidate = format!("{base}_{n}");
            if used.insert(candidate.clone()) {
                return candidate;
            }
            n += 1;
        }
    }

    fn warn_ignored_classes(&mut self) {
        for t in &self.doc.tables {
            if t.rows.is_empty() || MAPPED_CLASSES.contains(&t.class.as_str()) {
                continue;
            }
            self.warnings.push(format!(
                "`{}` table ignored ({} rows): not mapped to the distribution model",
                t.class,
                t.rows.len()
            ));
        }
    }
}

// ---- free helpers ----------------------------------------------------------

/// The namespaces `unique_name` deduplicates within.
#[derive(Clone, Copy)]
enum Namespace {
    Bus,
    Linecode,
    Elem,
}

impl Namespace {
    fn fallback(self) -> &'static str {
        match self {
            Namespace::Bus => "bus",
            Namespace::Linecode => "lc",
            Namespace::Elem => "elem",
        }
    }
}

impl BusAccum {
    fn seed(raw: &str) -> Self {
        BusAccum {
            id: raw.to_string(),
            phases: BTreeSet::new(),
            needs_neutral: false,
            uknom: 0.0,
        }
    }
}

/// The impedance representation a line/section carries, chosen by the priority
/// ladder in [`Reader::line_imp`]. `Explicit` matrices are natural-coordinate
/// totals (may be asymmetric and up to 4-wire); `Sequence` is symmetrical
/// component data reconstructed by Fortescue; `Geometry` is a bare geometry
/// type the reader skips.
enum LineImp {
    Explicit {
        r: Mat,
        x: Mat,
        g: Mat,
        b: Mat,
        n: usize,
    },
    Sequence {
        r1: f64,
        x1: f64,
        c1: f64,
        r0: f64,
        x0: f64,
        c0: f64,
        sline: f64,
        nph: usize,
        has_zero: bool,
    },
    Geometry,
}

/// The running aggregate of a line's phase-domain impedance, in total ohms and
/// siemens; divided by the aggregate length into per-metre at emission.
#[derive(Default)]
struct LineAccum {
    n_conductors: usize,
    r: Vec<Vec<f64>>,
    x: Vec<Vec<f64>>,
    g_half: Vec<Vec<f64>>,
    b_half: Vec<Vec<f64>>,
    length: f64,
    i_max: Option<f64>,
}

impl LineAccum {
    fn ensure(&mut self, i: usize) {
        let need = (i + 1).max(self.n_conductors);
        for m in [&mut self.r, &mut self.x, &mut self.g_half, &mut self.b_half] {
            while m.len() < need {
                m.push(vec![0.0; need]);
            }
            for row in m.iter_mut() {
                while row.len() < need {
                    row.push(0.0);
                }
            }
        }
    }

    /// Divide the totals by the aggregate length to per-metre, and place the
    /// shunt halves at both ends.
    fn into_linecode(mut self, name: &str, fline: f64) -> DistLineCode {
        let n = self.n_conductors.max(1);
        self.ensure(n - 1);
        let len = if self.length > 0.0 { self.length } else { 1.0 };
        let scale = |m: &[Vec<f64>]| -> Mat {
            m.iter()
                .map(|row| row.iter().map(|&v| v / len).collect())
                .collect()
        };
        let mut code = DistLineCode::new(name, scale(&self.r), scale(&self.x));
        let g_half = scale(&self.g_half);
        let b_half = scale(&self.b_half);
        code.g_from.clone_from(&g_half);
        code.g_to = g_half;
        code.b_from.clone_from(&b_half);
        code.b_to = b_half;
        if let Some(imax) = self.i_max {
            code.i_max = Some(vec![imax * fline; n]);
        }
        code
    }
}

/// Fortescue self/mutual pair from a positive- and zero-sequence value:
/// `self = (z0 + 2 z1)/3`, `mutual = (z0 - z1)/3`.
fn seq_pair(z1: f64, z0: f64) -> (f64, f64) {
    ((z0 + 2.0 * z1) / 3.0, (z0 - z1) / 3.0)
}

/// Phase terminal names, appending the grounded neutral when `neutral`.
fn phase_terminals(phases: &[u8], neutral: bool) -> Vec<String> {
    let mut m: Vec<String> = phases.iter().map(u8::to_string).collect();
    if neutral {
        m.push(NEUTRAL.to_string());
    }
    m
}

/// An `n`x`n` diagonal matrix with `v` on the diagonal.
fn diag_matrix(n: usize, v: f64) -> Mat {
    let mut m = vec![vec![0.0; n]; n];
    for (i, row) in m.iter_mut().enumerate() {
        row[i] = v;
    }
    m
}

/// The winding connection from a DGS vector-group code: leading `D` is delta,
/// everything else (`Y`, `YN`, `Z`, `ZN`) is wye. The bool marks a grounded
/// neutral (`N` suffix).
fn winding_conn(code: Option<&str>) -> (WindingConn, bool) {
    let c = code.unwrap_or("Y").trim().to_ascii_uppercase();
    let grounded = c.ends_with('N');
    if c.starts_with('D') {
        (WindingConn::Delta, false)
    } else {
        (WindingConn::Wye, grounded)
    }
}

/// The two terminal positions and switch states of a two-terminal element.
fn two_terminals(topo: &Topology, id: &str) -> Option<(usize, usize, bool, bool)> {
    let conns = topo.conns.get(id)?;
    let from = conns.iter().find(|c| c.side == 0)?;
    let to = conns.iter().find(|c| c.side == 1)?;
    Some((from.term_pos, to.term_pos, from.closed, to.closed))
}

/// The terminal position, switch state, and pinned phase of a one-terminal
/// element.
fn one_terminal(topo: &Topology, id: &str) -> Option<(usize, bool, Option<u8>)> {
    let conns = topo.conns.get(id)?;
    let c = conns
        .iter()
        .find(|c| c.side == 0)
        .or_else(|| conns.first())?;
    Some((c.term_pos, c.closed, c.phase))
}

fn missing_type(element: &str, typ: &str) -> Error {
    Error::FormatRead {
        format: FMT,
        message: format!(
            "line/transformer {element:?} references type {typ:?} that is not in the file; \
             re-export from PowerFactory with types included"
        ),
    }
}

/// Structural columns present on every element row.
fn is_structural(name: &str) -> bool {
    matches!(
        name,
        "ID" | "FID" | "OP" | "loc_name" | "fold_id" | "chr_name"
    )
}

/// Unmapped, non-structural columns of a mapped element, as `dgs_<attr>`.
fn extras(t: &DgsTable, r: &DgsRow, consumed: &[&str]) -> Extras {
    let mut ex = Extras::new();
    for (i, col) in t.columns.iter().enumerate() {
        if is_structural(col) || consumed.contains(&col.as_str()) {
            continue;
        }
        if let Some(Some(v)) = r.cells.get(i) {
            ex.insert(format!("dgs_{col}"), Value::String(v.clone()));
        }
    }
    ex
}

/// Transformer extras: the element row's leftovers plus the vector-group clock
/// and magnetizing data the distribution model has no field for.
fn xf_extras(t: &DgsTable, r: &DgsRow, tt: &DgsTable, tr: &DgsRow) -> Extras {
    let mut ex = extras(
        t,
        r,
        &[
            "typ_id", "nntap", "outserv", "ntnum", "ratfac", "cgnd_h", "cgnd_l",
        ],
    );
    for name in ["nt2ag", "curmg", "pfe", "tr2cn_h", "tr2cn_l"] {
        if let Some(v) = cell(tt, tr, name) {
            ex.insert(format!("dgs_{name}"), Value::String(v.to_string()));
        }
    }
    ex
}

/// Sanitize a DGS name into an identifier safe for dss bus/element names and
/// JSON keys: keep alphanumerics, `_` and `-`, map the rest to `_`.
fn sanitize(raw: &str) -> String {
    let s: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    s.trim_matches('_').to_string()
}

/// Classes the reader maps or consumes structurally; everything else warns.
const MAPPED_CLASSES: &[&str] = &[
    "General",
    "ElmNet",
    "ElmTerm",
    "ElmLod",
    "ElmLodlv",
    "ElmLodmv",
    "ElmShnt",
    "ElmSym",
    "ElmGenstat",
    "ElmPvsys",
    "ElmAsm",
    "ElmXnet",
    "ElmLne",
    "ElmLnesec",
    "ElmTow",
    "ElmCoup",
    "ElmTr2",
    "ElmTr3",
    "TypLod",
    "TypLne",
    "TypTow",
    "TypGeo",
    "TypCabsys",
    "TypTr2",
    "TypTr3",
    "StaCubic",
    "StaSwitch",
    "Matrix",
    "VecDouble",
];
