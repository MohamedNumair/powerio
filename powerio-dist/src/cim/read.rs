//! Distribution CIM into a multiconductor [`DistNetwork`].
//!
//! `ConnectivityNode` is the bus; each conducting equipment's `Terminal`s tie
//! it to nodes and its per-phase child objects (`ACLineSegmentPhase`, …) give
//! the conductors. Impedance comes from `PerLengthPhaseImpedance` (a matrix,
//! ohm/m — a direct fit for [`DistLineCode`]) or `PerLengthSequenceImpedance`
//! (sequence components expanded to a phase matrix). Everything the mapping
//! does not consume is counted per class into the parse warnings.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::xml::{CimDocument, CimObject, PropValue, parse_cimxml};
use crate::model::{
    Configuration, DistBus, DistLine, DistLineCode, DistLoad, DistNetwork, DistShunt,
    DistSourceFormat, DistSwitch, DistTransformer, Mat, VoltageSource, Winding, WindingConn,
};
use crate::{Error, Result};

const FMT: &str = "distribution CIM";

/// Parse a distribution CIM document (CIMXML text).
///
/// # Errors
/// [`Error::Xml`] on malformed XML, or when the document declares no CIM
/// namespace or has no `ConnectivityNode` records.
pub fn parse_cim_str(text: &str) -> Result<DistNetwork> {
    let mut net = parse_cim_documents(vec![parse_cimxml(text)?], None)?;
    // One document has a byte-exact echo; merged multi-file sets do not.
    net.source = Some(Arc::new(text.to_string()));
    Ok(net)
}

/// Parse a distribution CIM file, or a directory of CIM instance files read as
/// one case.
///
/// # Errors
/// See [`parse_cim_str`]; also [`Error::Io`] on unreadable paths.
pub fn parse_cim_file(path: impl AsRef<Path>) -> Result<DistNetwork> {
    let path = path.as_ref();
    if path.is_dir() {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(path)
            .map_err(|source| Error::Io {
                path: path.display().to_string(),
                source,
            })?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("xml"))
            .collect();
        paths.sort();
        let stem = path.file_name().and_then(|s| s.to_str());
        return parse_cim_paths(&paths, stem);
    }
    let stem = path.file_stem().and_then(|s| s.to_str());
    parse_cim_paths(std::slice::from_ref(&path.to_path_buf()), stem)
}

/// Cheap directory sniff: any `.xml` whose head looks like a CIM RDF file with
/// distribution equipment.
pub(crate) fn dir_has_cim(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("xml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let head = &text[..text.len().min(8192)];
        if head.contains("rdf-syntax")
            && head.contains("CIM")
            && (head.contains("ConnectivityNode") || head.contains("ACLineSegment"))
        {
            return true;
        }
    }
    false
}

pub(crate) fn parse_cim_paths(paths: &[PathBuf], name_hint: Option<&str>) -> Result<DistNetwork> {
    if paths.is_empty() {
        return Err(Error::Xml {
            format: FMT,
            message: "no .xml instance files to read".into(),
        });
    }
    let mut docs = Vec::new();
    let mut sole_text = None;
    for path in paths {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.display().to_string(),
            source,
        })?;
        docs.push(parse_cimxml(&text)?);
        sole_text = if paths.len() == 1 { Some(text) } else { None };
    }
    let mut net = parse_cim_documents(docs, name_hint)?;
    if let Some(text) = sole_text {
        net.source = Some(Arc::new(text));
    }
    Ok(net)
}

// ---------------------------------------------------------------------------
// Merged object store
// ---------------------------------------------------------------------------

struct Merged {
    id: String,
    class: String,
    props: Vec<(String, PropValue)>,
}

struct Store {
    objects: Vec<Merged>,
    by_id: HashMap<String, usize>,
}

impl Store {
    fn merge(&mut self, doc: CimDocument) {
        for CimObject { class, id, props } in doc.objects {
            if id.is_empty() {
                continue;
            }
            if let Some(&at) = self.by_id.get(&id) {
                self.objects[at].props.extend(props);
            } else {
                self.by_id.insert(id.clone(), self.objects.len());
                self.objects.push(Merged { id, class, props });
            }
        }
    }

    fn class_of(&self, id: &str) -> Option<&str> {
        self.by_id
            .get(id)
            .map(|&at| self.objects[at].class.as_str())
    }

    fn of_class<'a>(&'a self, class: &'a str) -> impl Iterator<Item = &'a str> {
        self.objects
            .iter()
            .filter(move |o| o.class == class)
            .map(|o| o.id.as_str())
    }

    fn prop(&self, id: &str, key: &str) -> Option<&PropValue> {
        let &at = self.by_id.get(id)?;
        self.objects[at]
            .props
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    fn text(&self, id: &str, key: &str) -> Option<&str> {
        self.prop(id, key).map(PropValue::as_str)
    }

    fn f(&self, id: &str, key: &str) -> Option<f64> {
        self.text(id, key)?.trim().parse().ok()
    }

    fn boolean(&self, id: &str, key: &str) -> Option<bool> {
        match self.text(id, key)?.trim() {
            "true" | "TRUE" | "True" | "1" => Some(true),
            "false" | "FALSE" | "False" | "0" => Some(false),
            _ => None,
        }
    }

    fn refv(&self, id: &str, key: &str) -> Option<&str> {
        match self.prop(id, key)? {
            PropValue::Ref(target) => Some(target),
            PropValue::Text(_) => None,
        }
    }

    /// The `value` tail of an `EnumClass.value` reference.
    fn enum_value(&self, id: &str, key: &str) -> Option<&str> {
        self.refv(id, key)?.rsplit('.').next()
    }

    fn name(&self, id: &str) -> String {
        self.text(id, "IdentifiedObject.name")
            .map_or_else(|| id.to_string(), str::to_string)
    }
}

// ---------------------------------------------------------------------------
// Phase mapping
// ---------------------------------------------------------------------------

/// A single `SinglePhaseKind` to its wire-coordinate terminal name.
fn phase_terminal(phase: &str) -> Option<&'static str> {
    match phase {
        "A" | "s1" => Some("1"),
        "B" | "s2" => Some("2"),
        "C" => Some("3"),
        "N" => Some("4"),
        _ => None,
    }
}

/// Expand a `PhaseCode`/phase string (`"A"`, `"ABC"`, `"ABCN"`, `"AB"`,
/// `"s1"`, `"s12"`, `"s1s2"`) into terminal names in canonical order.
fn expand_phase_code(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |t: &str| {
        let t = t.to_string();
        if !out.contains(&t) {
            out.push(t);
        }
    };
    let mut chars = code.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            'A' => push("1"),
            'B' => push("2"),
            'C' => push("3"),
            'N' => push("4"),
            's' => {
                // s1, s2, or s12.
                match chars.peek() {
                    Some('1') => {
                        chars.next();
                        push("1");
                        if chars.peek() == Some(&'2') {
                            chars.next();
                            push("2");
                        }
                    }
                    Some('2') => {
                        chars.next();
                        push("2");
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

fn parse_cim_documents(docs: Vec<CimDocument>, name_hint: Option<&str>) -> Result<DistNetwork> {
    let mut store = Store {
        objects: Vec::new(),
        by_id: HashMap::new(),
    };
    let mut cim_namespace = None;
    let mut description = None;
    for doc in docs {
        if cim_namespace.is_none() {
            cim_namespace.clone_from(&doc.cim_namespace);
        }
        if description.is_none() {
            description = doc.header.as_ref().and_then(|h| h.description.clone());
        }
        store.merge(doc);
    }
    if cim_namespace.is_none() {
        return Err(Error::Xml {
            format: FMT,
            message: "no CIM namespace declared; not a CIM document".into(),
        });
    }

    let mut warnings = Vec::new();
    let wiring = Wiring::build(&store);
    let mut mapper = Mapper {
        store: &store,
        wiring,
        warnings: &mut warnings,
    };

    // Elements first; each owns its own terminal map (including the wye
    // neutral). Buses are then derived from the terminals the elements
    // actually present, so a wye load's bus gains the grounded neutral it
    // connects to — matching the dss reader's materialized-neutral model.
    let node_names = mapper.connectivity_nodes();
    if node_names.is_empty() {
        return Err(Error::Xml {
            format: FMT,
            message: "no ConnectivityNode records; not a node-based CIM feeder".into(),
        });
    }
    let (linecodes, lines) = mapper.lines();
    let loads = mapper.loads();
    let shunts = mapper.shunts();
    let switches = mapper.switches();
    let transformers = mapper.transformers();
    let sources = mapper.sources();
    let buses = derive_buses(
        &node_names,
        &lines,
        &switches,
        &loads,
        &shunts,
        &transformers,
        &sources,
    );
    warn_unmapped(&store, &mut warnings);

    let base_frequency = store
        .of_class("BaseFrequency")
        .next()
        .and_then(|f| store.f(f, "BaseFrequency.frequency"))
        .unwrap_or(crate::dss::defaults::BASE_FREQUENCY);

    let name = description
        .filter(|d| !d.is_empty())
        .or_else(|| name_hint.map(str::to_string));
    let mut net = DistNetwork {
        name,
        base_frequency,
        buses,
        linecodes,
        lines,
        switches,
        transformers,
        loads,
        shunts,
        sources,
        warnings,
        source_format: Some(DistSourceFormat::Cim),
        source: Some(Arc::new(String::new())),
        ..DistNetwork::default()
    };
    // No byte-exact echo: a merged multi-file CIM set has no single source
    // text. Drop the placeholder so the echo tier never fires.
    net.source = None;
    Ok(net)
}

/// Terminal wiring: equipment → terminals (sequence order), terminal → node.
struct Wiring {
    of_equipment: HashMap<String, Vec<String>>,
    node_of: HashMap<String, String>,
}

impl Wiring {
    fn build(store: &Store) -> Wiring {
        let mut of_equipment: HashMap<String, Vec<(f64, String)>> = HashMap::new();
        let mut node_of = HashMap::new();
        for id in store.of_class("Terminal") {
            if let Some(eq) = store.refv(id, "Terminal.ConductingEquipment") {
                let seq = store
                    .f(id, "ACDCTerminal.sequenceNumber")
                    .or_else(|| store.f(id, "Terminal.sequenceNumber"))
                    .unwrap_or(1.0);
                of_equipment
                    .entry(eq.to_string())
                    .or_default()
                    .push((seq, id.to_string()));
            }
            if let Some(cn) = store.refv(id, "Terminal.ConnectivityNode") {
                node_of.insert(id.to_string(), cn.to_string());
            }
        }
        let of_equipment = of_equipment
            .into_iter()
            .map(|(eq, mut terms)| {
                terms.sort_by(|a, b| a.0.total_cmp(&b.0));
                (eq, terms.into_iter().map(|(_, t)| t).collect())
            })
            .collect();
        Wiring {
            of_equipment,
            node_of,
        }
    }

    fn terminals(&self, equipment: &str) -> &[String] {
        self.of_equipment.get(equipment).map_or(&[], Vec::as_slice)
    }

    fn node(&self, terminal: &str) -> Option<&str> {
        self.node_of.get(terminal).map(String::as_str)
    }

    /// The connectivity node at the equipment's terminal of the given
    /// sequence index (0-based).
    fn node_at(&self, equipment: &str, index: usize) -> Option<&str> {
        self.node(self.terminals(equipment).get(index)?)
    }
}

struct Mapper<'a> {
    store: &'a Store,
    wiring: Wiring,
    warnings: &'a mut Vec<String>,
}

impl Mapper<'_> {
    /// Per-phase child objects of `parent` (via `parentRef`), each a
    /// `(terminal_name, id)` pair in canonical terminal order.
    fn phase_children(&self, class: &str, parent_ref: &str, parent: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for id in self.store.of_class(class) {
            if self.store.refv(id, parent_ref) != Some(parent) {
                continue;
            }
            let Some(phase) = self.store.enum_value(id, &format!("{class}.phase")) else {
                continue;
            };
            if let Some(term) = phase_terminal(phase) {
                out.push((term.to_string(), id.to_string()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The terminal names an equipment presents, from its phase children if
    /// any, else its terminal `phases` code, else a default.
    fn terminal_map(&self, equipment: &str, phase_class: &str, default: &[&str]) -> Vec<String> {
        let children = self.phase_children(
            phase_class,
            &format!("{phase_class}.{}", phase_class.trim_end_matches("Phase")),
            equipment,
        );
        if !children.is_empty() {
            return children.into_iter().map(|(t, _)| t).collect();
        }
        // Composite phase code on the first terminal.
        if let Some(term) = self.wiring.terminals(equipment).first() {
            if let Some(code) = self.store.enum_value(term, "Terminal.phases") {
                let expanded = expand_phase_code(code);
                if !expanded.is_empty() {
                    return expanded;
                }
            }
        }
        default.iter().map(|s| (*s).to_string()).collect()
    }

    /// Every connectivity node as `(bus_name, cim_mrid)`, in document order.
    fn connectivity_nodes(&self) -> Vec<(String, String)> {
        self.store
            .of_class("ConnectivityNode")
            .map(|cn| (self.store.name(cn), cn.to_string()))
            .collect()
    }

    /// The bus id (its CIM name) a connectivity node maps to.
    fn bus_name(&self, cn: &str) -> String {
        self.store.name(cn)
    }

    fn lines(&mut self) -> (Vec<DistLineCode>, Vec<DistLine>) {
        let mut linecodes: Vec<DistLineCode> = Vec::new();
        let mut seen_lc: BTreeMap<String, String> = BTreeMap::new();
        let mut lines = Vec::new();
        // Two passes would borrow self mutably twice; collect ids first.
        let ids: Vec<String> = self
            .store
            .of_class("ACLineSegment")
            .map(str::to_string)
            .collect();
        for id in ids {
            let (Some(from), Some(to)) = (
                self.wiring.node_at(&id, 0).map(|c| self.bus_name(c)),
                self.wiring.node_at(&id, 1).map(|c| self.bus_name(c)),
            ) else {
                self.warnings.push(format!(
                    "ACLineSegment {}: needs two terminals on connectivity nodes; skipped",
                    self.store.name(&id)
                ));
                continue;
            };
            let map = self.terminal_map(&id, "ACLineSegmentPhase", &["1", "2", "3"]);
            let nphase = map.len().max(1);
            let length = self
                .store
                .f(&id, "Conductor.length")
                .filter(|l| *l > 0.0)
                .unwrap_or_else(|| {
                    self.warnings.push(format!(
                        "ACLineSegment {}: no positive length; using 1 m",
                        self.store.name(&id)
                    ));
                    1.0
                });
            let lc_name = self.linecode_for(&id, nphase, length, &mut linecodes, &mut seen_lc);
            let mut line = DistLine::new(
                self.store.name(&id),
                from,
                to,
                map.clone(),
                map,
                lc_name,
                length,
            );
            line.extras.insert("cim_mrid".into(), id.clone().into());
            lines.push(line);
        }
        (linecodes, lines)
    }

    /// Resolve (or synthesize) the linecode name for a line, appending a new
    /// [`DistLineCode`] when this impedance has not been seen.
    fn linecode_for(
        &mut self,
        line: &str,
        nphase: usize,
        length: f64,
        linecodes: &mut Vec<DistLineCode>,
        seen: &mut BTreeMap<String, String>,
    ) -> String {
        if let Some(pli) = self
            .store
            .refv(line, "ACLineSegment.PerLengthImpedance")
            .map(str::to_string)
        {
            if let Some(name) = seen.get(&pli) {
                return name.clone();
            }
            let name = self.store.name(&pli);
            if let Some((r, x, b)) = self.per_length_matrices(&pli, nphase) {
                let mut lc = DistLineCode::new(name.clone(), r, x);
                let half: Mat = b
                    .iter()
                    .map(|row| row.iter().map(|v| v / 2.0).collect())
                    .collect();
                lc.b_from.clone_from(&half);
                lc.b_to = half;
                linecodes.push(lc);
                seen.insert(pli, name.clone());
                return name;
            }
        }
        // Inline total ohms on the segment, per-length via the length.
        let name = format!("{}_lc", self.store.name(line));
        let r_tot = self.store.f(line, "ACLineSegment.r").unwrap_or(0.0);
        let x_tot = self.store.f(line, "ACLineSegment.x").unwrap_or(0.0);
        let b_tot = self.store.f(line, "ACLineSegment.bch").unwrap_or(0.0);
        let diag = |v: f64| {
            let mut m = vec![vec![0.0; nphase]; nphase];
            for (i, row) in m.iter_mut().enumerate() {
                row[i] = v / length;
            }
            m
        };
        let mut lc = DistLineCode::new(name.clone(), diag(r_tot), diag(x_tot));
        let half = diag(b_tot / 2.0);
        lc.b_from.clone_from(&half);
        lc.b_to = half;
        linecodes.push(lc);
        name
    }

    /// The `n×n` (r, x, b) matrices for a `PerLengthImpedance`, from the phase
    /// matrix data or expanded from sequence components. ohm/m and S/m.
    #[allow(clippy::many_single_char_names)] // r, x, b are the impedance matrix names
    fn per_length_matrices(&self, pli: &str, nphase: usize) -> Option<(Mat, Mat, Mat)> {
        match self.store.class_of(pli) {
            Some("PerLengthPhaseImpedance") => {
                let count = self
                    .store
                    .f(pli, "PerLengthPhaseImpedance.conductorCount")
                    .map_or(nphase, |c| c as usize);
                let n = count.max(nphase).max(1);
                let mut r = vec![vec![0.0; n]; n];
                let mut x = vec![vec![0.0; n]; n];
                let mut b = vec![vec![0.0; n]; n];
                for d in self.store.of_class("PhaseImpedanceData") {
                    if self
                        .store
                        .refv(d, "PhaseImpedanceData.PerLengthPhaseImpedance")
                        != Some(pli)
                    {
                        continue;
                    }
                    let (Some(row), Some(col)) = (
                        self.store
                            .f(d, "PhaseImpedanceData.row")
                            .map(|v| v as usize),
                        self.store
                            .f(d, "PhaseImpedanceData.column")
                            .map(|v| v as usize),
                    ) else {
                        continue;
                    };
                    if row < 1 || col < 1 || row > n || col > n {
                        continue;
                    }
                    let (i, j) = (row - 1, col - 1);
                    let rv = self.store.f(d, "PhaseImpedanceData.r").unwrap_or(0.0);
                    let xv = self.store.f(d, "PhaseImpedanceData.x").unwrap_or(0.0);
                    let bv = self.store.f(d, "PhaseImpedanceData.b").unwrap_or(0.0);
                    r[i][j] = rv;
                    r[j][i] = rv;
                    x[i][j] = xv;
                    x[j][i] = xv;
                    b[i][j] = bv;
                    b[j][i] = bv;
                }
                Some((r, x, b))
            }
            Some("PerLengthSequenceImpedance") => {
                let n = nphase.max(1);
                let r1 = self
                    .store
                    .f(pli, "PerLengthSequenceImpedance.r")
                    .unwrap_or(0.0);
                let x1 = self
                    .store
                    .f(pli, "PerLengthSequenceImpedance.x")
                    .unwrap_or(0.0);
                let b1 = self
                    .store
                    .f(pli, "PerLengthSequenceImpedance.bch")
                    .unwrap_or(0.0);
                let r0 = self
                    .store
                    .f(pli, "PerLengthSequenceImpedance.r0")
                    .unwrap_or(r1);
                let x0 = self
                    .store
                    .f(pli, "PerLengthSequenceImpedance.x0")
                    .unwrap_or(x1);
                let b0 = self
                    .store
                    .f(pli, "PerLengthSequenceImpedance.b0ch")
                    .unwrap_or(b1);
                Some((
                    sequence_to_phase(r0, r1, n),
                    sequence_to_phase(x0, x1, n),
                    sequence_to_phase(b0, b1, n),
                ))
            }
            _ => None,
        }
    }

    fn loads(&mut self) -> Vec<DistLoad> {
        let ids: Vec<String> = self
            .store
            .of_class("EnergyConsumer")
            .map(str::to_string)
            .collect();
        let mut loads = Vec::new();
        for id in ids {
            let Some(bus) = self.wiring.node_at(&id, 0).map(|c| self.bus_name(c)) else {
                self.warnings.push(format!(
                    "EnergyConsumer {}: no terminal on a connectivity node; skipped",
                    self.store.name(&id)
                ));
                continue;
            };
            let config = match self.store.enum_value(&id, "EnergyConsumer.phaseConnection") {
                Some("D") => Configuration::Delta,
                Some("I") => Configuration::SinglePhase,
                _ => Configuration::Wye,
            };
            let children = self.phase_children(
                "EnergyConsumerPhase",
                "EnergyConsumerPhase.EnergyConsumer",
                &id,
            );
            let (map, p_nom, q_nom) = if children.is_empty() {
                let map = self.terminal_map(&id, "EnergyConsumerPhase", &["1", "2", "3"]);
                let n = map.len().max(1) as f64;
                let p = self.store.f(&id, "EnergyConsumer.p").unwrap_or(0.0) / n;
                let q = self.store.f(&id, "EnergyConsumer.q").unwrap_or(0.0) / n;
                (map.clone(), vec![p; map.len()], vec![q; map.len()])
            } else {
                let mut map = Vec::new();
                let mut p_nom = Vec::new();
                let mut q_nom = Vec::new();
                for (term, phase_id) in children {
                    map.push(term);
                    p_nom.push(
                        self.store
                            .f(&phase_id, "EnergyConsumerPhase.p")
                            .unwrap_or(0.0),
                    );
                    q_nom.push(
                        self.store
                            .f(&phase_id, "EnergyConsumerPhase.q")
                            .unwrap_or(0.0),
                    );
                }
                (map, p_nom, q_nom)
            };
            // Wye and single-phase loads connect their phases plus the
            // grounded neutral; the writers read `phases` as terminals − 1.
            let map = with_neutral(map, config);
            let mut load = DistLoad::new(self.store.name(&id), bus, map, config, p_nom, q_nom);
            load.extras.insert("cim_mrid".into(), id.clone().into());
            loads.push(load);
        }
        loads
    }

    fn shunts(&mut self) -> Vec<DistShunt> {
        let ids: Vec<String> = self
            .store
            .of_class("LinearShuntCompensator")
            .map(str::to_string)
            .collect();
        let mut shunts = Vec::new();
        for id in ids {
            let Some(bus) = self.wiring.node_at(&id, 0).map(|c| self.bus_name(c)) else {
                continue;
            };
            let children = self.phase_children(
                "ShuntCompensatorPhase",
                "ShuntCompensatorPhase.ShuntCompensator",
                &id,
            );
            let (map, g_diag, b_diag) = if children.is_empty() {
                let map = self.terminal_map(&id, "ShuntCompensatorPhase", &["1", "2", "3"]);
                let g = self
                    .store
                    .f(&id, "LinearShuntCompensator.gPerSection")
                    .unwrap_or(0.0);
                let b = self
                    .store
                    .f(&id, "LinearShuntCompensator.bPerSection")
                    .unwrap_or(0.0);
                (map.clone(), vec![g; map.len()], vec![b; map.len()])
            } else {
                let mut map = Vec::new();
                let mut g_diag = Vec::new();
                let mut b_diag = Vec::new();
                for (term, phase_id) in children {
                    map.push(term);
                    g_diag.push(
                        self.store
                            .f(&phase_id, "ShuntCompensatorPhase.gPerSection")
                            .unwrap_or(0.0),
                    );
                    b_diag.push(
                        self.store
                            .f(&phase_id, "ShuntCompensatorPhase.bPerSection")
                            .unwrap_or(0.0),
                    );
                }
                (map, g_diag, b_diag)
            };
            let mut shunt = DistShunt::new(
                self.store.name(&id),
                bus,
                map,
                diagonal(&g_diag),
                diagonal(&b_diag),
            );
            shunt.extras.insert("cim_mrid".into(), id.clone().into());
            shunts.push(shunt);
        }
        shunts
    }

    fn switches(&mut self) -> Vec<DistSwitch> {
        let ids: Vec<String> = SWITCH_CLASSES
            .iter()
            .flat_map(|c| {
                self.store
                    .of_class(c)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut switches = Vec::new();
        for id in ids {
            let (Some(from), Some(to)) = (
                self.wiring.node_at(&id, 0).map(|c| self.bus_name(c)),
                self.wiring.node_at(&id, 1).map(|c| self.bus_name(c)),
            ) else {
                continue;
            };
            let map = self.terminal_map(&id, "SwitchPhase", &["1", "2", "3"]);
            let open = self
                .store
                .boolean(&id, "Switch.open")
                .or_else(|| self.store.boolean(&id, "Switch.normalOpen"))
                .unwrap_or(false);
            let mut switch =
                DistSwitch::new(self.store.name(&id), from, to, map.clone(), map, open);
            switch.extras.insert("cim_mrid".into(), id.clone().into());
            switches.push(switch);
        }
        switches
    }

    fn transformers(&mut self) -> Vec<DistTransformer> {
        // Two-winding PowerTransformer with PowerTransformerEnd records.
        let ends_of = self.group_transformer_ends();
        let mut transformers = Vec::new();
        let mut tank_warned = false;
        for xf in self
            .store
            .of_class("PowerTransformer")
            .map(str::to_string)
            .collect::<Vec<_>>()
        {
            let Some(mut ends) = ends_of.get(&xf).cloned() else {
                if !tank_warned && self.store.of_class("TransformerTank").next().is_some() {
                    tank_warned = true;
                }
                self.warnings.push(format!(
                    "PowerTransformer {}: no PowerTransformerEnd records (TransformerTank/\
                     TankInfo form is not mapped yet); skipped",
                    self.store.name(&xf)
                ));
                continue;
            };
            ends.sort_by(|a, b| a.0.total_cmp(&b.0));
            if ends.len() != 2 {
                self.warnings.push(format!(
                    "PowerTransformer {}: {} windings (only two-winding is mapped); skipped",
                    self.store.name(&xf),
                    ends.len()
                ));
                continue;
            }
            let phases = self.transformer_phase_count(&ends);
            let Some(windings) = self.build_windings(&ends, phases) else {
                self.warnings.push(format!(
                    "PowerTransformer {}: an end has no terminal on a connectivity node; skipped",
                    self.store.name(&xf)
                ));
                continue;
            };
            let xsc = vec![self.mesh_reactance_pct(&ends)];
            let mut transformer = DistTransformer::new(self.store.name(&xf), windings, xsc, phases);
            transformer
                .extras
                .insert("cim_mrid".into(), xf.clone().into());
            transformers.push(transformer);
        }
        transformers
    }

    fn group_transformer_ends(&self) -> HashMap<String, Vec<(f64, String)>> {
        let mut ends_of: HashMap<String, Vec<(f64, String)>> = HashMap::new();
        for end in self.store.of_class("PowerTransformerEnd") {
            if let Some(xf) = self.store.refv(end, "PowerTransformerEnd.PowerTransformer") {
                let n = self.store.f(end, "TransformerEnd.endNumber").unwrap_or(1.0);
                ends_of
                    .entry(xf.to_string())
                    .or_default()
                    .push((n, end.to_string()));
            }
        }
        ends_of
    }

    fn transformer_phase_count(&self, ends: &[(f64, String)]) -> usize {
        // Phase count from the first end's terminal phases, default 3.
        for (_, end) in ends {
            if let Some(term) = self
                .store
                .refv(end, "TransformerEnd.Terminal")
                .map(str::to_string)
            {
                if let Some(code) = self.store.enum_value(&term, "Terminal.phases") {
                    let n = expand_phase_code(code).iter().filter(|t| *t != "4").count();
                    if n > 0 {
                        return n;
                    }
                }
            }
        }
        3
    }

    fn build_windings(&self, ends: &[(f64, String)], phases: usize) -> Option<Vec<Winding>> {
        let default_map: Vec<String> = (1..=phases).map(|i| i.to_string()).collect();
        let mut windings = Vec::new();
        for (_, end) in ends {
            let term = self.store.refv(end, "TransformerEnd.Terminal")?;
            let cn = self.wiring.node(term)?;
            let bus = self.bus_name(cn);
            let conn = match self
                .store
                .enum_value(end, "PowerTransformerEnd.connectionKind")
            {
                Some("D") => WindingConn::Delta,
                _ => WindingConn::Wye,
            };
            let map = self
                .store
                .enum_value(term, "Terminal.phases")
                .map(expand_phase_code)
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| default_map.clone());
            let v_ref = self
                .store
                .f(end, "PowerTransformerEnd.ratedU")
                .unwrap_or(0.0);
            let s_rating = self
                .store
                .f(end, "PowerTransformerEnd.ratedS")
                .unwrap_or(0.0);
            let map = if conn == WindingConn::Wye {
                with_neutral(map, Configuration::Wye)
            } else {
                map
            };
            let mut winding = Winding::new(bus, map, conn, v_ref, s_rating);
            // Per-end resistance (ohm) to percent of the winding base.
            if let Some(r) = self.store.f(end, "PowerTransformerEnd.r") {
                if let Some(base) = z_base(v_ref, s_rating) {
                    winding.r_pct = r / base * 100.0;
                }
            }
            windings.push(winding);
        }
        Some(windings)
    }

    /// Short-circuit reactance between the two windings, percent of the base.
    /// Prefers a `TransformerMeshImpedance`; falls back to the sum of the ends'
    /// own `x`.
    fn mesh_reactance_pct(&self, ends: &[(f64, String)]) -> f64 {
        let (v_ref, s_rating) = ends.first().map_or((0.0, 0.0), |(_, e)| {
            (
                self.store.f(e, "PowerTransformerEnd.ratedU").unwrap_or(0.0),
                self.store.f(e, "PowerTransformerEnd.ratedS").unwrap_or(0.0),
            )
        });
        let Some(base) = z_base(v_ref, s_rating) else {
            return 0.0;
        };
        for mesh in self.store.of_class("TransformerMeshImpedance") {
            if let Some(x) = self.store.f(mesh, "TransformerMeshImpedance.x") {
                return x / base * 100.0;
            }
        }
        let x_sum: f64 = ends
            .iter()
            .map(|(_, e)| self.store.f(e, "PowerTransformerEnd.x").unwrap_or(0.0))
            .sum();
        x_sum / base * 100.0
    }

    fn sources(&mut self) -> Vec<VoltageSource> {
        let ids: Vec<String> = self
            .store
            .of_class("EnergySource")
            .map(str::to_string)
            .collect();
        let mut sources = Vec::new();
        for id in ids {
            let Some(bus) = self.wiring.node_at(&id, 0).map(|c| self.bus_name(c)) else {
                continue;
            };
            let map = self.terminal_map(&id, "EnergySourcePhase", &["1", "2", "3"]);
            let n = map.len().max(1);
            // voltageMagnitude is line-to-line RMS; the per-terminal source is
            // line-to-neutral at 120-degree spacing (three-phase) offset by
            // the source angle.
            let vll = self
                .store
                .f(&id, "EnergySource.voltageMagnitude")
                .or_else(|| self.store.f(&id, "EnergySource.nominalVoltage"))
                .unwrap_or(0.0);
            let v_ln = if n >= 2 { vll / 3f64.sqrt() } else { vll };
            let base_angle = self
                .store
                .f(&id, "EnergySource.voltageAngle")
                .unwrap_or(0.0);
            let v_magnitude = vec![v_ln; map.len()];
            let v_angle = (0..map.len())
                .map(|i| base_angle - (i as f64) * 2.0 * std::f64::consts::PI / 3.0)
                .collect();
            let map = with_neutral(map, Configuration::Wye);
            let mut source =
                VoltageSource::new(self.store.name(&id), bus, map, v_magnitude, v_angle);
            source.extras.insert("cim_mrid".into(), id.clone().into());
            sources.push(source);
        }
        sources
    }
}

const SWITCH_CLASSES: [&str; 7] = [
    "LoadBreakSwitch",
    "Breaker",
    "Recloser",
    "Sectionaliser",
    "Fuse",
    "Disconnector",
    "Jumper",
];

/// Classes the mapping consumes (no "unmapped" warning).
const CONSUMED: [&str; 20] = [
    "ConnectivityNode",
    "Terminal",
    "BaseVoltage",
    "BaseFrequency",
    "ACLineSegment",
    "ACLineSegmentPhase",
    "PerLengthPhaseImpedance",
    "PerLengthSequenceImpedance",
    "PhaseImpedanceData",
    "EnergyConsumer",
    "EnergyConsumerPhase",
    "LinearShuntCompensator",
    "ShuntCompensatorPhase",
    "EnergySource",
    "EnergySourcePhase",
    "PowerTransformer",
    "PowerTransformerEnd",
    "TransformerMeshImpedance",
    "RatioTapChanger",
    "Location",
];

fn warn_unmapped(store: &Store, warnings: &mut Vec<String>) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for object in &store.objects {
        let class = object.class.as_str();
        let consumed = CONSUMED.contains(&class)
            || SWITCH_CLASSES.contains(&class)
            || class.ends_with("Info")
            || matches!(
                class,
                "IEC61970CIMVersion"
                    | "GeographicalRegion"
                    | "SubGeographicalRegion"
                    | "Line"
                    | "Substation"
                    | "VoltageLevel"
                    | "Bay"
                    | "Feeder"
                    | "CoordinateSystem"
                    | "PositionPoint"
                    | "Analog"
                    | "Discrete"
                    | "Name"
                    | "NameType"
                    | "NameTypeAuthority"
            );
        if !consumed {
            *counts.entry(class).or_default() += 1;
        }
    }
    for (class, count) in counts {
        warnings.push(format!(
            "{count} {class} object(s) have no multiconductor mapping and are not carried"
        ));
    }
}

/// A wye or single-phase element's terminal map connects its phases plus the
/// grounded neutral (node 4). Delta maps have no neutral.
fn with_neutral(mut map: Vec<String>, config: Configuration) -> Vec<String> {
    if matches!(config, Configuration::Wye | Configuration::SinglePhase)
        && !map.iter().any(|t| t == "4")
    {
        map.push("4".to_string());
    }
    map
}

/// Buses from the connectivity nodes, with each bus's terminals taken from the
/// terminals the elements actually present there (so a wye element's grounded
/// neutral appears on its bus). A neutral terminal reads as perfectly
/// grounded, the wire-coordinate convention shared with the dss reader.
fn derive_buses(
    nodes: &[(String, String)],
    lines: &[DistLine],
    switches: &[DistSwitch],
    loads: &[DistLoad],
    shunts: &[DistShunt],
    transformers: &[DistTransformer],
    sources: &[VoltageSource],
) -> Vec<DistBus> {
    let mut used: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut add = |bus: &str, terms: &[String]| {
        used.entry(bus.to_ascii_lowercase())
            .or_default()
            .extend(terms.iter().cloned());
    };
    for l in lines {
        add(&l.bus_from, &l.terminal_map_from);
        add(&l.bus_to, &l.terminal_map_to);
    }
    for s in switches {
        add(&s.bus_from, &s.terminal_map_from);
        add(&s.bus_to, &s.terminal_map_to);
    }
    for l in loads {
        add(&l.bus, &l.terminal_map);
    }
    for s in shunts {
        add(&s.bus, &s.terminal_map);
    }
    for t in transformers {
        for w in &t.windings {
            add(&w.bus, &w.terminal_map);
        }
    }
    for s in sources {
        add(&s.bus, &s.terminal_map);
    }
    nodes
        .iter()
        .map(|(name, mrid)| {
            let mut terminals: Vec<String> = used
                .get(&name.to_ascii_lowercase())
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default();
            terminals.sort();
            let grounded = if terminals.iter().any(|t| t == "4") {
                vec!["4".to_string()]
            } else {
                Vec::new()
            };
            let mut bus = DistBus::new(name.clone(), terminals);
            bus.grounded = grounded;
            bus.extras.insert("cim_mrid".into(), mrid.clone().into());
            bus
        })
        .collect()
}

/// A diagonal siemens matrix from per-conductor values.
fn diagonal(values: &[f64]) -> Mat {
    let n = values.len();
    let mut m = vec![vec![0.0; n]; n];
    for (i, &v) in values.iter().enumerate() {
        m[i][i] = v;
    }
    m
}

/// Impedance base `V²/S` (ohm) for a winding, or None if degenerate.
fn z_base(v_ref: f64, s_rating: f64) -> Option<f64> {
    (v_ref > 0.0 && s_rating > 0.0).then_some(v_ref * v_ref / s_rating)
}

/// Expand zero/positive-sequence scalars into an `n×n` phase matrix:
/// self = (z0 + 2·z1)/3 on the diagonal, mutual = (z0 − z1)/3 off it.
fn sequence_to_phase(z0: f64, z1: f64, n: usize) -> Mat {
    let self_z = (z0 + 2.0 * z1) / 3.0;
    let mutual = (z0 - z1) / 3.0;
    let mut m = vec![vec![mutual; n]; n];
    for (i, row) in m.iter_mut().enumerate() {
        row[i] = self_z;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_codes_expand_to_terminal_names() {
        assert_eq!(expand_phase_code("ABC"), ["1", "2", "3"]);
        assert_eq!(expand_phase_code("ABCN"), ["1", "2", "3", "4"]);
        assert_eq!(expand_phase_code("AB"), ["1", "2"]);
        assert_eq!(expand_phase_code("s1s2"), ["1", "2"]);
        assert_eq!(expand_phase_code("s12"), ["1", "2"]);
        assert_eq!(expand_phase_code("C"), ["3"]);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn sequence_expands_to_a_symmetric_phase_matrix() {
        let m = sequence_to_phase(6.0, 3.0, 3);
        // self = (6 + 6)/3 = 4, mutual = (6 - 3)/3 = 1.
        assert_eq!(m[0][0], 4.0);
        assert_eq!(m[0][1], 1.0);
        assert_eq!(m[1][0], 1.0);
    }
}
