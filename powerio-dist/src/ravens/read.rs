//! MG-RAVENS JSON → [`DistNetwork`].
//!
//! The reader walks the document's class-hierarchy nesting into a flat
//! object index (so it does not depend on the exact abstract-class levels a
//! producer chose), then maps the distribution profile's concrete classes.
//! Objects with no typed mapping are preserved as [`UntypedObject`]s carrying
//! their raw JSON and root path, so a RAVENS → RAVENS conversion re-emits
//! them in place and every other target warns about them by name.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::model::{
    Configuration, DistBus, DistGenerator, DistIbr, DistLine, DistLineCode, DistLoad,
    DistLoadVoltageModel, DistNetwork, DistShunt, DistSourceFormat, DistSwitch, DistTransformer,
    Extras, IbrPrimeMover, IbrTopology, Mat, UntypedObject, VoltageSource, Winding, WindingConn,
};
use crate::{Error, Result};

use super::{MRID_KEY, NORMAL_OPEN_KEY, natural_key, phase_terminal};

const FMT: &str = "MG-RAVENS";

/// Object types that only appear in balanced (transmission-profile) RAVENS
/// documents — the MATPOWER-derived conventions the `powerio` crate reads.
/// Their presence routes the whole document away from this multiconductor
/// reader.
const BALANCED_TYPES: [&str; 3] = [
    "AlgorithmSettings",
    "ProducerCostFunction",
    "PowerTransformerEnd",
];

/// Parse an MG-RAVENS JSON file into a [`DistNetwork`].
///
/// # Errors
/// [`Error::Io`] when the file cannot be read; otherwise as
/// [`parse_ravens_str`].
pub fn parse_ravens_file(path: impl AsRef<std::path::Path>) -> Result<DistNetwork> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.display().to_string(),
        source,
    })?;
    parse_ravens_str(&text)
}

/// Parse an in-memory MG-RAVENS JSON document into a [`DistNetwork`].
///
/// # Errors
/// [`Error::Json`] on malformed JSON, a non-object top level, or a balanced
/// transmission-profile document (which belongs to the `powerio` crate's
/// reader).
pub fn parse_ravens_str(text: &str) -> Result<DistNetwork> {
    let root_value: Value = serde_json::from_str(text).map_err(|e| Error::Json {
        format: FMT,
        message: e.to_string(),
    })?;
    let Value::Object(root) = root_value else {
        return Err(Error::Json {
            format: FMT,
            message: "top level is not an object".into(),
        });
    };

    let doc = Doc::collect(&root);
    reject_balanced(&doc)?;

    let mut mapper = Mapper {
        doc: &doc,
        net: DistNetwork::new(),
        node_terminals: BTreeMap::new(),
        node_grounded: BTreeSet::new(),
        consumed: BTreeSet::new(),
    };
    mapper.net.warnings.push(
        "MG-RAVENS carries no system frequency; base_frequency assumes the OpenDSS default \
         (60 Hz)"
            .into(),
    );
    mapper.run();

    let mut net = mapper.net;
    net.source = Some(Arc::new(text.to_owned()));
    net.source_format = Some(DistSourceFormat::RavensJson);
    Ok(net)
}

// ---------------------------------------------------------------------------
// Document walk
// ---------------------------------------------------------------------------

/// Every recorded object: its concrete `Ravens.cimObjectType`, the hash key
/// it is filed under (the pointer-resolution name), the root-relative hash
/// key path it was found at, whether it sits inline inside another object,
/// and its properties.
struct Obj {
    kind: String,
    name: String,
    /// Hash keys from the document root down to (excluding) the object's own
    /// key; array hops contribute nothing. Empty for inline objects.
    path: Vec<String>,
    /// True when the object is nested inside another collected object (an
    /// inline `Terminals` array entry, a transformer tank, …): it rides with
    /// its parent and is never preserved on its own.
    nested: bool,
    props: Map<String, Value>,
}

/// Name-keyed object index, walked out of the class-hierarchy nesting.
struct Doc {
    objects: Vec<Obj>,
}

impl Doc {
    fn collect(root: &Map<String, Value>) -> Doc {
        let mut objects = Vec::new();
        let mut path = Vec::new();
        for (key, value) in root {
            walk(Some(key), value, &mut path, false, &mut objects);
        }
        Doc { objects }
    }

    fn of_kind<'s>(&'s self, kind: &str) -> impl Iterator<Item = &'s Obj> {
        // Own the kind so the returned iterator's lifetime is tied to the
        // document, not to the caller's `kind` borrow.
        let kind = kind.to_owned();
        self.objects.iter().filter(move |o| o.kind == kind)
    }

    /// Objects of one concrete kind in natural name order. Hash tables carry
    /// no order, and lexicographic order puts `line10` before `line2`; the
    /// upstream numbering conventions make natural order the source order.
    fn sorted_of_kind(&self, kind: &str) -> Vec<&Obj> {
        let mut objects: Vec<&Obj> = self.of_kind(kind).collect();
        objects.sort_by_key(|o| natural_key(&o.name));
        objects
    }

    /// Resolve a `Class::'name'` pointer. RAVENS pointers name the target's
    /// hash key; the class in the pointer may be an ancestor of the object's
    /// concrete type, so matching is by name with the class as a tiebreaker.
    fn resolve(&self, pointer: &Value) -> Option<&Obj> {
        let (class, name) = pointer_parts(pointer)?;
        self.objects
            .iter()
            .find(|o| o.name == name && o.kind == class)
            .or_else(|| self.objects.iter().find(|o| o.name == name))
    }
}

fn walk(
    name: Option<&str>,
    value: &Value,
    path: &mut Vec<String>,
    nested: bool,
    out: &mut Vec<Obj>,
) {
    match value {
        Value::Object(map) => {
            let is_object = map.get("Ravens.cimObjectType").and_then(Value::as_str);
            if let Some(kind) = is_object {
                let fallback = map
                    .get("IdentifiedObject.name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                out.push(Obj {
                    kind: kind.to_owned(),
                    name: name.unwrap_or(fallback).to_owned(),
                    path: path.clone(),
                    nested,
                    props: map.clone(),
                });
            }
            let child_nested = nested || is_object.is_some();
            for (key, child) in map {
                path.push(name.unwrap_or_default().to_owned());
                walk(Some(key), child, path, child_nested, out);
                path.pop();
            }
        }
        Value::Array(items) => {
            for item in items {
                // Array hops keep the current path; entries are inline.
                if let Some(name) = name {
                    path.push(name.to_owned());
                    walk(None, item, path, nested, out);
                    path.pop();
                } else {
                    walk(None, item, path, nested, out);
                }
            }
        }
        _ => {}
    }
}

fn pointer_parts(pointer: &Value) -> Option<(&str, &str)> {
    let text = pointer.as_str()?;
    let (class, rest) = text.split_once("::'")?;
    Some((class, rest.strip_suffix('\'')?))
}

fn pointer_name(pointer: &Value) -> Option<&str> {
    pointer_parts(pointer).map(|(_, name)| name)
}

fn fnum(obj: &Obj, key: &str) -> Option<f64> {
    obj.props.get(key).and_then(Value::as_f64)
}

fn f_or(obj: &Obj, key: &str, default: f64) -> f64 {
    fnum(obj, key).unwrap_or(default)
}

fn fbool(obj: &Obj, key: &str) -> Option<bool> {
    obj.props.get(key).and_then(Value::as_bool)
}

fn fstr<'a>(obj: &'a Obj, key: &str) -> Option<&'a str> {
    obj.props.get(key).and_then(Value::as_str)
}

fn mrid(obj: &Obj) -> Option<String> {
    fstr(obj, "IdentifiedObject.mRID").map(str::to_owned)
}

fn kind_suffix(value: &str) -> &str {
    value.rsplit('.').next().unwrap_or(value)
}

/// One equipment terminal: sequence number, connectivity node, phase
/// letters, and the current-limit set it points at.
struct TerminalEntry {
    node: String,
    phases: Vec<String>,
    limit_set: Option<String>,
}

/// The inline `ConductingEquipment.Terminals` of an equipment (or a
/// transformer end), in sequence-number order.
fn terminal_entries(props: &Map<String, Value>) -> Vec<TerminalEntry> {
    let Some(Value::Array(items)) = props.get("ConductingEquipment.Terminals") else {
        return Vec::new();
    };
    let mut with_seq: Vec<(u64, TerminalEntry)> = items
        .iter()
        .filter_map(|t| {
            let t = t.as_object()?;
            let seq = t
                .get("ACDCTerminal.sequenceNumber")
                .and_then(Value::as_u64)
                .unwrap_or(1);
            let node = t
                .get("Terminal.ConnectivityNode")
                .and_then(pointer_name)?
                .to_owned();
            let phases = t
                .get("Terminal.phases")
                .and_then(Value::as_str)
                .map(super::expand_phase_code)
                .unwrap_or_default();
            let limit_set = t
                .get("ACDCTerminal.OperationalLimitSet")
                .and_then(pointer_name)
                .map(str::to_owned);
            Some((
                seq,
                TerminalEntry {
                    node,
                    phases,
                    limit_set,
                },
            ))
        })
        .collect();
    with_seq.sort_by_key(|(seq, _)| *seq);
    with_seq.into_iter().map(|(_, t)| t).collect()
}

/// Phase letters → terminal names, dropping codes the crate has no terminal
/// for. Neutral (`N`) maps to terminal 4 like everywhere else in the crate.
fn letters_to_terminals(letters: &[String]) -> Vec<String> {
    letters
        .iter()
        .filter_map(|l| phase_terminal(l))
        .map(str::to_owned)
        .collect()
}

fn hot_terminals(letters: &[String]) -> Vec<String> {
    letters_to_terminals(letters)
        .into_iter()
        .filter(|t| t != "4")
        .collect()
}

/// The continuous (norm) and short-duration (emergency) current limits and
/// the low/high voltage limits of an `OperationalLimitSet`, distinguished by
/// each value's `OperationalLimitType` direction and acceptable duration
/// (the upstream converters use 5e9 s for continuous limits).
#[derive(Default)]
struct Limits {
    norm_amps: Option<f64>,
    emerg_amps: Option<f64>,
    v_low: Option<f64>,
    v_high: Option<f64>,
}

fn limit_set(doc: &Doc, set_name: &str) -> Limits {
    let mut out = Limits::default();
    let Some(set) = doc
        .of_kind("OperationalLimitSet")
        .find(|o| o.name == set_name)
    else {
        return out;
    };
    let Some(Value::Array(values)) = set.props.get("OperationalLimitSet.OperationalLimitValue")
    else {
        return out;
    };
    for value in values {
        let Some(value) = value.as_object() else {
            continue;
        };
        let kind = value
            .get("Ravens.cimObjectType")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let ty = value
            .get("OperationalLimit.OperationalLimitType")
            .and_then(|p| doc.resolve(p));
        let direction = ty
            .and_then(|t| fstr(t, "OperationalLimitType.direction"))
            .map(kind_suffix)
            .unwrap_or_default();
        let continuous = ty
            .and_then(|t| fnum(t, "OperationalLimitType.acceptableDuration"))
            .is_none_or(|d| d >= 1e9);
        match kind {
            "CurrentLimit" => {
                let amps = value.get("CurrentLimit.value").and_then(Value::as_f64);
                if continuous {
                    out.norm_amps = out.norm_amps.or(amps);
                } else {
                    out.emerg_amps = out.emerg_amps.or(amps);
                }
            }
            "VoltageLimit" => {
                let volts = value.get("VoltageLimit.value").and_then(Value::as_f64);
                match direction {
                    "low" => out.v_low = out.v_low.or(volts),
                    "high" => out.v_high = out.v_high.or(volts),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

fn reject_balanced(doc: &Doc) -> Result<()> {
    let found: Vec<&str> = BALANCED_TYPES
        .iter()
        .filter(|t| doc.objects.iter().any(|o| &o.kind == *t))
        .copied()
        .collect();
    if found.is_empty() {
        return Ok(());
    }
    Err(Error::Json {
        format: FMT,
        message: format!(
            "document carries balanced transmission-profile objects ({}); the multiconductor \
             distribution reader would invent per-phase detail. Balanced MG-RAVENS documents \
             are read by the transmission hub in the `powerio` crate",
            found.join(", ")
        ),
    })
}

// ---------------------------------------------------------------------------
// Mapper
// ---------------------------------------------------------------------------

struct Mapper<'a> {
    doc: &'a Doc,
    net: DistNetwork,
    /// Terminal numbers each connectivity node was seen with.
    node_terminals: BTreeMap<String, BTreeSet<u8>>,
    node_grounded: BTreeSet<String>,
    /// Concrete kinds a typed mapping claimed (nested helpers included).
    consumed: BTreeSet<&'static str>,
}

impl Mapper<'_> {
    fn warn(&mut self, msg: impl Into<String>) {
        self.net.warnings.push(msg.into());
    }

    fn defaulted(&mut self, class: &str, name: &str, field: &'static str) {
        self.net
            .defaulted
            .entry(format!("{class}.{name}"))
            .or_default()
            .push(field);
    }

    fn claim(&mut self, kinds: &[&'static str]) {
        self.consumed.extend(kinds.iter().copied());
    }

    fn use_terminals(&mut self, node: &str, terminals: &[String]) {
        let entry = self.node_terminals.entry(node.to_owned()).or_default();
        for t in terminals {
            if let Ok(n) = t.parse::<u8>() {
                entry.insert(n);
            }
        }
    }

    fn ground_node(&mut self, node: &str) {
        self.node_terminals
            .entry(node.to_owned())
            .or_default()
            .insert(4);
        self.node_grounded.insert(node.to_owned());
    }

    /// The nominal (line-to-line) voltage of the equipment's
    /// `ConductingEquipment.BaseVoltage` pointer, if any.
    fn base_voltage(&self, obj: &Obj) -> Option<f64> {
        let base = obj.props.get("ConductingEquipment.BaseVoltage")?;
        fnum(self.doc.resolve(base)?, "BaseVoltage.nominalVoltage")
    }

    fn run(&mut self) {
        self.claim(&[
            "IEC61970CIMVersion",
            "RavensVersion",
            "ConnectivityNode",
            "BaseVoltage",
            "Terminal",
            "OperationalLimitSet",
            "OperationalLimitType",
            "VoltageLimit",
            "CurrentLimit",
        ]);
        self.map_linecodes();
        self.map_lines();
        self.map_switches();
        self.map_loads();
        self.map_shunts();
        self.map_machines();
        self.map_power_electronics();
        self.map_sources();
        self.map_transformers();
        self.finish_buses();
        self.preserve_unclaimed();
    }

    // ----- per-length impedance ------------------------------------------

    fn map_linecodes(&mut self) {
        self.claim(&["PerLengthPhaseImpedance", "PhaseImpedanceData"]);
        for obj in self.doc.sorted_of_kind("PerLengthPhaseImpedance") {
            let rows: Vec<(usize, usize, f64, f64, f64, f64)> = obj
                .props
                .get("PerLengthPhaseImpedance.PhaseImpedanceData")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|row| {
                            let row = row.as_object()?;
                            let at = |key: &str| row.get(key).and_then(Value::as_f64);
                            let i = at("PhaseImpedanceData.row")? as usize;
                            let j = at("PhaseImpedanceData.column")? as usize;
                            Some((
                                i,
                                j,
                                at("PhaseImpedanceData.r").unwrap_or(0.0),
                                at("PhaseImpedanceData.x").unwrap_or(0.0),
                                at("PhaseImpedanceData.b").unwrap_or(0.0),
                                at("PhaseImpedanceData.g").unwrap_or(0.0),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let n = fnum(obj, "PerLengthPhaseImpedance.conductorCount").map_or_else(
                || rows.iter().map(|&(i, j, ..)| i.max(j)).max().unwrap_or(0),
                |c| c as usize,
            );
            if n == 0 {
                let name = obj.name.clone();
                self.warn(format!(
                    "PerLengthPhaseImpedance {name}: no impedance rows; linecode skipped"
                ));
                continue;
            }
            let zero: Mat = vec![vec![0.0; n]; n];
            let (mut r_mat, mut x_mat, mut b_mat, mut g_mat) =
                (zero.clone(), zero.clone(), zero.clone(), zero);
            for &(row, col, rv, xv, bv, gv) in &rows {
                if row == 0 || col == 0 || row > n || col > n {
                    continue;
                }
                let (row, col) = (row - 1, col - 1);
                for (m, v) in [
                    (&mut r_mat, rv),
                    (&mut x_mat, xv),
                    (&mut b_mat, bv),
                    (&mut g_mat, gv),
                ] {
                    m[row][col] = v;
                    m[col][row] = v;
                }
            }
            // The π-model shunt halves at each end; division by two is exact,
            // so the writer's `b_from + b_to` reproduces the source bits.
            let half = |m: &Mat| -> Mat {
                m.iter()
                    .map(|row| row.iter().map(|v| v / 2.0).collect())
                    .collect()
            };
            let mut code = DistLineCode::new(obj.name.clone(), r_mat, x_mat);
            code.n_conductors = n;
            code.b_from = half(&b_mat);
            code.b_to = half(&b_mat);
            code.g_from = half(&g_mat);
            code.g_to = half(&g_mat);
            code.extras = extras_with_mrid(obj);
            self.net.linecodes.push(code);
        }
    }

    // ----- lines ----------------------------------------------------------

    #[allow(clippy::too_many_lines)] // one segment maps in one pass: terminals, phases, ampacity
    fn map_lines(&mut self) {
        self.claim(&["ACLineSegment", "ACLineSegmentPhase"]);
        for obj in self.doc.sorted_of_kind("ACLineSegment") {
            let name = obj.name.clone();
            let terminals = terminal_entries(&obj.props);
            let (Some(from), Some(to)) = (terminals.first(), terminals.get(1)) else {
                self.keep_untyped(obj, "fewer than two terminals");
                continue;
            };
            let Some(code_name) = obj
                .props
                .get("ACLineSegment.PerLengthImpedance")
                .and_then(pointer_name)
                .map(str::to_owned)
            else {
                // Wire/spacing catalog construction (WireSpacingInfo +
                // OverheadWireInfo) needs Carson's equations, which belong to
                // the tool that owns them; the object is preserved verbatim.
                self.keep_untyped(obj, "no PerLengthImpedance (wire-table lines are roadmap)");
                continue;
            };

            // Conductor order: the ACLineSegmentPhase records in sequence
            // order, else the terminal's phase code.
            let mut phase_rows: Vec<(u64, String)> = obj
                .props
                .get("ACLineSegment.ACLineSegmentPhase")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|p| {
                            let p = p.as_object()?;
                            let seq = p
                                .get("ACLineSegmentPhase.sequenceNumber")
                                .and_then(Value::as_u64)
                                .unwrap_or(1);
                            let letter = p
                                .get("ACLineSegmentPhase.phase")
                                .and_then(Value::as_str)
                                .map(kind_suffix)?;
                            Some((seq, letter.to_owned()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            phase_rows.sort_by_key(|(seq, _)| *seq);
            let letters: Vec<String> = if phase_rows.is_empty() {
                from.phases.clone()
            } else {
                phase_rows.into_iter().map(|(_, l)| l).collect()
            };
            let mut map = hot_terminals(&letters);
            if map.is_empty() {
                let n = self
                    .net
                    .linecode(&code_name)
                    .map_or(3, |c| c.n_conductors.min(3));
                map = (1..=n).map(|k| k.to_string()).collect();
                self.defaulted("line", &name, "phases");
            }

            let length = f_or(obj, "Conductor.length", 1.0);
            if fnum(obj, "Conductor.length").is_none() {
                self.defaulted("line", &name, "length");
            }

            // Ampacity: the emergency limit is the model's `i_max` (the
            // dss reader's convention); the continuous limit rides in the
            // line's extras as `normamps`.
            let mut extras = extras_with_mrid(obj);
            if let Some(set) = &from.limit_set {
                let set_name = set.clone();
                let limits = limit_set(self.doc, &set_name);
                if let Some(norm) = limits.norm_amps {
                    extras.insert("normamps".into(), norm.into());
                }
                if let Some(emerg) = limits.emerg_amps {
                    let n = map.len();
                    let mut conflicts = false;
                    if let Some(code) = self.net.linecodes.iter_mut().find(|c| c.name == code_name)
                    {
                        let proposed = vec![emerg; n.max(code.n_conductors)];
                        match &code.i_max {
                            Some(existing) if *existing != proposed => conflicts = true,
                            Some(_) => {}
                            None => code.i_max = Some(proposed),
                        }
                    }
                    if conflicts {
                        self.warn(format!(
                            "line {name}: emergency ampacity {emerg} conflicts with an \
                             earlier line sharing linecode {code_name}; keeping the first \
                             value"
                        ));
                    }
                }
            }

            self.use_terminals(&from.node, &map);
            self.use_terminals(&to.node, &map);
            let mut line = DistLine::new(
                name,
                from.node.clone(),
                to.node.clone(),
                map.clone(),
                map,
                code_name,
                length,
            );
            line.extras = extras;
            self.net.lines.push(line);
        }
    }

    // ----- switches -------------------------------------------------------

    fn map_switches(&mut self) {
        self.claim(&[
            "Switch",
            "LoadBreakSwitch",
            "Breaker",
            "Recloser",
            "SwitchPhase",
        ]);
        let mut switches: Vec<&Obj> = ["Switch", "LoadBreakSwitch", "Breaker", "Recloser"]
            .iter()
            .flat_map(|k| self.doc.of_kind(k))
            .collect();
        switches.sort_by_key(|o| natural_key(&o.name));
        for obj in switches {
            let name = obj.name.clone();
            let terminals = terminal_entries(&obj.props);
            let (Some(from), Some(to)) = (terminals.first(), terminals.get(1)) else {
                self.keep_untyped(obj, "fewer than two terminals");
                continue;
            };
            // Per-phase side mappings when the document has them.
            let sides: Vec<(String, String)> = obj
                .props
                .get("Switch.SwitchPhase")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|p| {
                            let p = p.as_object()?;
                            let side = |key: &str| {
                                p.get(key)
                                    .and_then(Value::as_str)
                                    .map(kind_suffix)
                                    .and_then(phase_terminal)
                                    .map(str::to_owned)
                            };
                            Some((
                                side("SwitchPhase.phaseSide1")?,
                                side("SwitchPhase.phaseSide2")?,
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let (map_from, map_to) = if sides.is_empty() {
                (hot_terminals(&from.phases), hot_terminals(&to.phases))
            } else {
                sides.into_iter().unzip()
            };
            let (map_from, map_to) = if map_from.is_empty() || map_to.is_empty() {
                self.defaulted("switch", &name, "phases");
                let all = vec!["1".to_owned(), "2".to_owned(), "3".to_owned()];
                (all.clone(), all)
            } else {
                (map_from, map_to)
            };

            let open = fbool(obj, "Switch.open").unwrap_or(false);
            let mut extras = extras_with_mrid(obj);
            if let Some(normal) = fbool(obj, "Switch.normalOpen") {
                if normal != open {
                    extras.insert(NORMAL_OPEN_KEY.into(), normal.into());
                }
            }
            let i_max = from
                .limit_set
                .as_deref()
                .map(|set| limit_set(self.doc, set))
                .and_then(|l| {
                    if let Some(norm) = l.norm_amps {
                        extras.insert("normamps".into(), norm.into());
                    }
                    l.emerg_amps
                })
                .map(|amps| vec![amps; map_from.len()]);

            self.use_terminals(&from.node, &map_from);
            self.use_terminals(&to.node, &map_to);
            let mut switch = DistSwitch::new(
                name,
                from.node.clone(),
                to.node.clone(),
                map_from,
                map_to,
                open,
            );
            switch.i_max = i_max;
            switch.extras = extras;
            self.net.switches.push(switch);
        }
    }

    // ----- loads ----------------------------------------------------------

    #[allow(clippy::too_many_lines)] // one load maps in one pass: powers, phases, response, profile
    fn map_loads(&mut self) {
        // `EnergyConnectionProfile` is consumed into the load's dss-profile
        // extras (and regenerated from them on write), so it is claimed here
        // rather than preserved untyped — otherwise the write side would
        // emit it twice. The `EnergyConsumerSchedule` load shapes it names
        // are not modeled and stay untyped for the round trip.
        self.claim(&[
            "EnergyConsumer",
            "EnergyConsumerPhase",
            "LoadResponseCharacteristic",
            "EnergyConnectionProfile",
        ]);
        for obj in self.doc.sorted_of_kind("EnergyConsumer") {
            let name = obj.name.clone();
            let terminals = terminal_entries(&obj.props);
            let Some(at) = terminals.first() else {
                self.keep_untyped(obj, "no terminal");
                continue;
            };

            // Per-phase powers off the phase objects, kept in the document's
            // array order (a delta load's phase order is meaningful and the
            // writer emits it in terminal-map order); totals split evenly
            // when a document carries no phases.
            let mut per_phase: Vec<(String, f64, f64)> = obj
                .props
                .get("EnergyConsumer.EnergyConsumerPhase")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|p| {
                            let p = p.as_object()?;
                            let letter = p
                                .get("EnergyConsumerPhase.phase")
                                .and_then(Value::as_str)
                                .map(kind_suffix)?;
                            let terminal = phase_terminal(letter)?.to_owned();
                            Some((
                                terminal,
                                p.get("EnergyConsumerPhase.p")
                                    .and_then(Value::as_f64)
                                    .unwrap_or(0.0),
                                p.get("EnergyConsumerPhase.q")
                                    .and_then(Value::as_f64)
                                    .unwrap_or(0.0),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let had_powers = !per_phase.is_empty() || fnum(obj, "EnergyConsumer.p").is_some();
            if per_phase.is_empty() {
                let hot = hot_terminals(&at.phases);
                let hot = if hot.is_empty() {
                    self.defaulted("load", &name, "phases");
                    vec!["1".to_owned(), "2".to_owned(), "3".to_owned()]
                } else {
                    hot
                };
                let n = hot.len() as f64;
                let (p, q) = (
                    f_or(obj, "EnergyConsumer.p", 0.0) / n,
                    f_or(obj, "EnergyConsumer.q", 0.0) / n,
                );
                per_phase = hot.into_iter().map(|t| (t, p, q)).collect();
            }
            let phases = per_phase.len();

            let conn = fstr(obj, "EnergyConsumer.phaseConnection").map_or("Y", kind_suffix);
            let delta = conn == "D";
            let configuration = if phases == 1 && !delta {
                Configuration::SinglePhase
            } else if delta {
                Configuration::Delta
            } else {
                Configuration::Wye
            };
            let grounded = fbool(obj, "EnergyConsumer.grounded").unwrap_or(!delta);

            let mut map: Vec<String> = per_phase.iter().map(|(t, ..)| t.clone()).collect();
            if delta && phases == 1 {
                // A one-phase delta load spans two hot terminals; the
                // terminal's phase code carries the pair.
                let pair = hot_terminals(&at.phases);
                if pair.len() == 2 {
                    map = pair;
                } else {
                    self.warn(format!(
                        "load {name}: single-phase delta needs a two-phase terminal code; \
                         kept untyped"
                    ));
                    self.keep_untyped(obj, "unresolvable delta terminals");
                    continue;
                }
            } else if !delta && grounded {
                map.push("4".to_owned());
                self.ground_node(&at.node);
            }

            let base = self.base_voltage(obj);
            let v_phase = base.map(|b| if delta { b } else { b / 3f64.sqrt() });
            let v_nom = if let Some(v) = v_phase {
                vec![v; phases]
            } else {
                self.warn(format!(
                    "load {name}: no BaseVoltage; nominal voltage left unset"
                ));
                Vec::new()
            };
            let voltage_model = self.load_voltage_model(obj, v_nom);
            if !had_powers {
                self.defaulted("load", &name, "kw");
            }

            let mut extras = extras_with_mrid(obj);
            if let Some(b) = base {
                // dss `kv` semantics: line-to-neutral for one phase, line-to-line
                // otherwise. Derived from the BaseVoltage, so recorded as such.
                let kv = if phases == 1 && !delta {
                    b / 3f64.sqrt() / 1e3
                } else {
                    b / 1e3
                };
                extras.insert("kv".into(), kv.into());
                extras.insert("phases".into(), (phases as u64).into());
                self.defaulted("load", &name, "kv");
            }
            self.attach_profiles(obj, &mut extras);

            self.use_terminals(&at.node, &map);
            let mut load = DistLoad::new(
                name,
                at.node.clone(),
                map,
                configuration,
                per_phase.iter().map(|&(_, p, _)| p).collect(),
                per_phase.iter().map(|&(_, _, q)| q).collect(),
            );
            load.voltage_model = voltage_model;
            load.extras = extras;
            self.net.loads.push(load);
        }
    }

    /// `LoadResponseCharacteristic` → the typed voltage model: the three
    /// canonical 100% anchors map to the constant models, exponent records
    /// to the exponential model, and anything mixed to ZIP.
    #[allow(clippy::float_cmp)] // the anchors are the exact literals 100.0 / 0.0 by construction
    fn load_voltage_model(&mut self, obj: &Obj, v_nom: Vec<f64>) -> DistLoadVoltageModel {
        let Some(response) = obj
            .props
            .get("EnergyConsumer.LoadResponse")
            .and_then(|p| self.doc.resolve(p))
        else {
            self.defaulted("load", &obj.name, "model");
            return DistLoadVoltageModel::ConstantPower { v_nom };
        };
        let at = |key: &str| f_or(response, key, 0.0);
        if fbool(response, "LoadResponseCharacteristic.exponentModel").unwrap_or(false) {
            let n = v_nom.len().max(1);
            return DistLoadVoltageModel::Exponential {
                v_nom,
                gamma_p: vec![at("LoadResponseCharacteristic.pVoltageExponent"); n],
                gamma_q: vec![at("LoadResponseCharacteristic.qVoltageExponent"); n],
            };
        }
        let (pz, pi, pp) = (
            at("LoadResponseCharacteristic.pConstantImpedance"),
            at("LoadResponseCharacteristic.pConstantCurrent"),
            at("LoadResponseCharacteristic.pConstantPower"),
        );
        let (qz, qi, qp) = (
            at("LoadResponseCharacteristic.qConstantImpedance"),
            at("LoadResponseCharacteristic.qConstantCurrent"),
            at("LoadResponseCharacteristic.qConstantPower"),
        );
        let anchor = |v: f64, others: [f64; 2]| v == 100.0 && others.iter().all(|&o| o == 0.0);
        if anchor(pp, [pz, pi]) && anchor(qp, [qz, qi]) {
            DistLoadVoltageModel::ConstantPower { v_nom }
        } else if anchor(pz, [pp, pi]) && anchor(qz, [qp, qi]) {
            DistLoadVoltageModel::ConstantImpedance { v_nom }
        } else if anchor(pi, [pp, pz]) && anchor(qi, [qp, qz]) {
            DistLoadVoltageModel::ConstantCurrent { v_nom }
        } else {
            let n = v_nom.len().max(1);
            DistLoadVoltageModel::Zip {
                v_nom,
                alpha_z: vec![pz / 100.0; n],
                alpha_i: vec![pi / 100.0; n],
                alpha_p: vec![pp / 100.0; n],
                beta_z: vec![qz / 100.0; n],
                beta_i: vec![qi / 100.0; n],
                beta_p: vec![qp / 100.0; n],
            }
        }
    }

    /// `EnergyConnectionProfile.dssDaily`/`dssYearly`/`dssSpectrum` back onto
    /// the load's dss-property extras, matched through the `LoadProfile`
    /// schedule name (the upstream converter links them by name).
    fn attach_profiles(&mut self, obj: &Obj, extras: &mut Extras) {
        let schedule = obj
            .props
            .get("EnergyConsumer.LoadProfile")
            .and_then(pointer_name)
            .map(str::to_owned);
        let Some(schedule) = schedule else { return };
        let profile = self.doc.of_kind("EnergyConnectionProfile").find(|p| {
            fstr(p, "EnergyConnectionProfile.dssDaily") == Some(schedule.as_str())
                || fstr(p, "EnergyConnectionProfile.dssYearly") == Some(schedule.as_str())
        });
        let Some(profile) = profile else {
            extras.insert("daily".into(), schedule.into());
            return;
        };
        for (prop, key) in [
            ("EnergyConnectionProfile.dssDaily", "daily"),
            ("EnergyConnectionProfile.dssYearly", "yearly"),
            ("EnergyConnectionProfile.dssSpectrum", "spectrum"),
        ] {
            if let Some(v) = fstr(profile, prop) {
                extras.insert(key.into(), v.into());
            }
        }
    }

    // ----- shunts ---------------------------------------------------------

    fn map_shunts(&mut self) {
        self.claim(&["LinearShuntCompensator"]);
        for obj in self.doc.sorted_of_kind("LinearShuntCompensator") {
            let name = obj.name.clone();
            let terminals = terminal_entries(&obj.props);
            let Some(at) = terminals.first() else {
                self.keep_untyped(obj, "no terminal");
                continue;
            };
            // A shunt can sit on the neutral alone (a grounding bank), so the
            // terminal phase code is read neutral-inclusive; the writer emits
            // it the same way.
            let map = letters_to_terminals(&at.phases);
            let map = if map.is_empty() {
                self.defaulted("shunt", &name, "phases");
                vec!["1".to_owned(), "2".to_owned(), "3".to_owned()]
            } else {
                map
            };
            let n = map.len();
            let delta = fstr(obj, "ShuntCompensator.phaseConnection").map(kind_suffix) == Some("D");
            if delta {
                self.warn(format!(
                    "shunt {name}: delta connection kept as per-phase diagonal susceptance; \
                     the off-diagonal geometry is not reconstructed"
                ));
            }
            let sections = fnum(obj, "ShuntCompensator.sections")
                .or_else(|| fnum(obj, "ShuntCompensator.normalSections"))
                .unwrap_or(1.0);
            let b_phase = f_or(obj, "LinearShuntCompensator.bPerSection", 0.0) * sections;
            let g_phase = f_or(obj, "LinearShuntCompensator.gPerSection", 0.0) * sections;
            let diag = |v: f64| -> Mat {
                (0..n)
                    .map(|i| (0..n).map(|j| if i == j { v } else { 0.0 }).collect())
                    .collect()
            };

            let mut extras = extras_with_mrid(obj);
            let nom_u = fnum(obj, "ShuntCompensator.nomU");
            if let Some(nom_u) = nom_u {
                // dss `kv` semantics: a wye bank's kv is line-to-line for 2-3
                // phases (nomU is the per-phase voltage), line-to-neutral or
                // across-the-branch otherwise.
                let kv = if !delta && n >= 2 {
                    nom_u * 3f64.sqrt() / 1e3
                } else {
                    nom_u / 1e3
                };
                extras.insert("kv".into(), kv.into());
                extras.insert("phases".into(), (n as u64).into());
                extras.insert(
                    "kvar".into(),
                    (b_phase * nom_u * nom_u * n as f64 / 1e3).into(),
                );
                self.defaulted("capacitor", &name, "kv");
                self.defaulted("capacitor", &name, "kvar");
            }
            if delta {
                extras.insert("conn".into(), "delta".into());
            } else {
                self.ground_node(&at.node);
            }

            self.use_terminals(&at.node, &map);
            let mut shunt =
                DistShunt::new(name, at.node.clone(), map, diag(g_phase), diag(b_phase));
            shunt.extras = extras;
            self.net.shunts.push(shunt);
        }
    }

    // ----- rotating machines ---------------------------------------------

    fn map_machines(&mut self) {
        self.claim(&[
            "SynchronousMachine",
            "AsynchronousMachine",
            "GeneratingUnit",
        ]);
        let mut machines: Vec<&Obj> = ["SynchronousMachine", "AsynchronousMachine"]
            .iter()
            .flat_map(|k| self.doc.of_kind(k))
            .collect();
        machines.sort_by_key(|o| natural_key(&o.name));
        for obj in machines {
            let name = obj.name.clone();
            let terminals = terminal_entries(&obj.props);
            let Some(at) = terminals.first() else {
                self.keep_untyped(obj, "no terminal");
                continue;
            };
            let hot = hot_terminals(&at.phases);
            let hot = if hot.is_empty() {
                self.defaulted("generator", &name, "phases");
                vec!["1".to_owned(), "2".to_owned(), "3".to_owned()]
            } else {
                hot
            };
            let phases = hot.len();
            let n = phases as f64;
            let per_phase = |total: f64| vec![total / n; phases];

            let mut map = hot;
            let configuration = if phases == 1 {
                Configuration::SinglePhase
            } else {
                Configuration::Wye
            };
            map.push("4".to_owned());

            let mut extras = extras_with_mrid(obj);
            if let Some(rated_u) = fnum(obj, "RotatingMachine.ratedU") {
                extras.insert("kv".into(), (rated_u / 1e3).into());
                extras.insert("phases".into(), (phases as u64).into());
            }
            if let Some(rated_s) = fnum(obj, "RotatingMachine.ratedS") {
                extras.insert("kva".into(), (rated_s / 1e3).into());
            }

            let mut machine = DistGenerator::new(
                name,
                at.node.clone(),
                map.clone(),
                configuration,
                per_phase(f_or(obj, "RotatingMachine.p", 0.0)),
                per_phase(f_or(obj, "RotatingMachine.q", 0.0)),
            );
            machine.q_max = fnum(obj, "SynchronousMachine.maxQ").map(per_phase);
            machine.q_min = fnum(obj, "SynchronousMachine.minQ").map(per_phase);
            if let Some(Value::Object(unit)) = obj.props.get("RotatingMachine.GeneratingUnit") {
                machine.p_max = unit
                    .get("GeneratingUnit.maxOperatingP")
                    .and_then(Value::as_f64)
                    .map(per_phase);
                machine.p_min = unit
                    .get("GeneratingUnit.minOperatingP")
                    .and_then(Value::as_f64)
                    .map(per_phase);
            }
            machine.extras = extras;
            self.use_terminals(&at.node, &map);
            self.net.generators.push(machine);
        }
    }

    // ----- power electronics (PV / storage) --------------------------------

    #[allow(clippy::too_many_lines)] // one connection maps in one pass: unit, ratings, limits
    fn map_power_electronics(&mut self) {
        self.claim(&[
            "PowerElectronicsConnection",
            "PhotoVoltaicUnit",
            "BatteryUnit",
            "BatteryUnitEfficiency",
        ]);
        for obj in self.doc.sorted_of_kind("PowerElectronicsConnection") {
            let name = obj.name.clone();
            let terminals = terminal_entries(&obj.props);
            let Some(at) = terminals.first() else {
                self.keep_untyped(obj, "no terminal");
                continue;
            };
            let hot = hot_terminals(&at.phases);
            let hot = if hot.is_empty() {
                self.defaulted("pvsystem", &name, "phases");
                vec!["1".to_owned(), "2".to_owned(), "3".to_owned()]
            } else {
                hot
            };
            let phases = hot.len();
            let n = phases as f64;
            let per_phase = |total: f64| vec![total / n; phases];

            let unit = obj
                .props
                .get("PowerElectronicsConnection.PowerElectronicsUnit")
                .and_then(Value::as_object);
            let unit_kind = unit
                .and_then(|u| u.get("Ravens.cimObjectType"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let prime_mover = match unit_kind {
                "PhotoVoltaicUnit" => IbrPrimeMover::Pv,
                "BatteryUnit" => IbrPrimeMover::Battery,
                _ => {
                    self.warn(format!(
                        "power electronics {name}: unit type `{unit_kind}` is not typed; \
                         treated as a generic inverter"
                    ));
                    IbrPrimeMover::Generic
                }
            };

            let rated_s = f_or(obj, "PowerElectronicsConnection.ratedS", 0.0);
            let rated_u = fnum(obj, "PowerElectronicsConnection.ratedU");
            let mut map = hot;
            map.push("4".to_owned());
            let topology = match phases {
                1 => IbrTopology::SinglePhase,
                _ => IbrTopology::ThreeLeg,
            };

            let mut extras = extras_with_mrid(obj);
            if let Some(u) = rated_u {
                extras.insert("kv".into(), (u / 1e3).into());
            }
            if let Some(q) = fnum(obj, "PowerElectronicsConnection.q") {
                extras.insert("kvar".into(), (q / 1e3).into());
            }
            if let Some(unit) = unit {
                for (prop, key) in [
                    ("BatteryUnit.ratedE", "ravens_rated_e"),
                    ("BatteryUnit.storedE", "ravens_stored_e"),
                ] {
                    if let Some(v) = unit.get(prop) {
                        extras.insert(key.into(), v.clone());
                    }
                }
                if let Some(eff) = unit.get("BatteryUnit.BatteryUnitEfficiency") {
                    extras.insert("ravens_battery_efficiency".into(), eff.clone());
                }
            }

            let mut ibr = DistIbr::new(
                name,
                at.node.clone(),
                map.clone(),
                topology,
                prime_mover,
                per_phase(rated_s),
            );
            ibr.p_avail = fnum(obj, "PowerElectronicsConnection.p");
            ibr.q_max = fnum(obj, "PowerElectronicsConnection.maxQ").map(per_phase);
            ibr.q_min = fnum(obj, "PowerElectronicsConnection.minQ").map(per_phase);
            if let Some(unit) = unit {
                ibr.p_max = unit
                    .get("PowerElectronicsUnit.maxP")
                    .and_then(Value::as_f64)
                    .map(per_phase);
                ibr.p_min = unit
                    .get("PowerElectronicsUnit.minP")
                    .and_then(Value::as_f64)
                    .map(per_phase);
            }
            // maxIFault is per-unit of rated current on the rated voltage.
            if let (Some(pu), Some(u)) =
                (fnum(obj, "PowerElectronicsConnection.maxIFault"), rated_u)
            {
                if rated_s > 0.0 && u > 0.0 {
                    let rated_amps = if phases == 1 {
                        rated_s / u
                    } else {
                        rated_s / (3f64.sqrt() * u)
                    };
                    ibr.i_max = Some(vec![pu * rated_amps; phases]);
                }
            }
            ibr.extras = extras;
            self.use_terminals(&at.node, &map);
            self.net.ibrs.push(ibr);
        }
    }

    // ----- sources ---------------------------------------------------------

    fn map_sources(&mut self) {
        self.claim(&["EnergySource"]);
        for obj in self.doc.sorted_of_kind("EnergySource") {
            let name = obj.name.clone();
            let terminals = terminal_entries(&obj.props);
            let Some(at) = terminals.first() else {
                self.keep_untyped(obj, "no terminal");
                continue;
            };
            let hot = hot_terminals(&at.phases);
            let hot = if hot.is_empty() {
                self.defaulted("vsource", &name, "phases");
                vec!["1".to_owned(), "2".to_owned(), "3".to_owned()]
            } else {
                hot
            };
            let phases = hot.len();
            let n = phases as f64;

            let magnitude = f_or(obj, "EnergySource.voltageMagnitude", 0.0);
            let angle = f_or(obj, "EnergySource.voltageAngle", 0.0);
            // The dss vsource conversion: one phase takes the magnitude
            // outright, otherwise per-phase is the chord of the n-gon
            // (sqrt(3) at n = 3), and angles space at -tau/n wrapped to
            // (-pi, pi].
            let v_ln = if phases == 1 {
                magnitude
            } else {
                magnitude / (2.0 * (std::f64::consts::PI / n).sin())
            };
            let mut v_magnitude = vec![v_ln; phases];
            let mut v_angle: Vec<f64> = (0..phases)
                .map(|k| {
                    let a = angle - std::f64::consts::TAU / n * k as f64;
                    let shifted = (a + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU);
                    if shifted <= 0.0 {
                        std::f64::consts::PI
                    } else {
                        shifted - std::f64::consts::PI
                    }
                })
                .collect();
            // The neutral conductor rides at ground.
            let mut map = hot;
            map.push("4".to_owned());
            v_magnitude.push(0.0);
            v_angle.push(0.0);

            let mut extras = extras_with_mrid(obj);
            let nominal = fnum(obj, "EnergySource.nominalVoltage");
            if let Some(nom) = nominal {
                extras.insert("basekv".into(), (nom / 1e3).into());
                let pu = if nom > 0.0 { magnitude / nom } else { 1.0 };
                if (pu - 1.0).abs() > 0.0 {
                    extras.insert("pu".into(), pu.into());
                }
            }
            extras.insert("angle".into(), angle.to_degrees().into());
            for (prop, key) in [
                ("EnergySource.r", "r1"),
                ("EnergySource.x", "x1"),
                ("EnergySource.r0", "r0"),
                ("EnergySource.x0", "x0"),
            ] {
                if let Some(v) = fnum(obj, prop) {
                    extras.insert(key.into(), v.into());
                }
            }

            self.use_terminals(&at.node, &map);
            self.ground_node(&at.node);
            let mut source = VoltageSource::new(name, at.node.clone(), map, v_magnitude, v_angle);
            source.extras = extras;
            self.net.sources.push(source);
        }
    }

    // ----- transformers -----------------------------------------------------

    #[allow(clippy::too_many_lines)] // one tank maps in one pass: ends, catalog, tests, taps
    fn map_transformers(&mut self) {
        self.claim(&[
            "PowerTransformer",
            "TransformerTank",
            "TransformerTankEnd",
            "TransformerTankInfo",
            "TransformerEndInfo",
            "ShortCircuitTest",
            "NoLoadTest",
        ]);
        // Each tank is one transformer, the dss modeling convention the
        // upstream converter mirrors (a three-tank PowerTransformer is a
        // bank of one-phase units).
        let mut tanks: Vec<&Obj> = self.doc.of_kind("TransformerTank").collect();
        tanks.sort_by_key(|o| natural_key(&o.name));
        for tank in tanks {
            let name = tank.name.clone();
            let info_name = tank
                .props
                .get("PowerSystemResource.AssetDatasheet")
                .and_then(pointer_name)
                .map(str::to_owned);
            let info = info_name.as_deref().and_then(|n| {
                self.doc
                    .of_kind("TransformerTankInfo")
                    .find(|o| o.name == n)
            });
            let Some(info) = info else {
                self.warn(format!(
                    "transformer tank {name}: no TransformerTankInfo datasheet; the tank has \
                     no voltages or impedances and is kept untyped"
                ));
                self.keep_untyped(tank, "missing datasheet");
                continue;
            };

            // Catalog records by end number.
            let end_infos: BTreeMap<u64, &Map<String, Value>> = info
                .props
                .get("TransformerTankInfo.TransformerEndInfos")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|e| {
                            let e = e.as_object()?;
                            let num = e
                                .get("TransformerEndInfo.endNumber")
                                .and_then(Value::as_u64)?;
                            Some((num, e))
                        })
                        .collect()
                })
                .unwrap_or_default();

            let mut ends: Vec<(u64, &Map<String, Value>)> = tank
                .props
                .get("TransformerTank.TransformerTankEnd")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|e| {
                            let e = e.as_object()?;
                            let num = e
                                .get("TransformerEnd.endNumber")
                                .and_then(Value::as_u64)
                                .unwrap_or(1);
                            Some((num, e))
                        })
                        .collect()
                })
                .unwrap_or_default();
            ends.sort_by_key(|(num, _)| *num);
            if ends.len() < 2 {
                self.warn(format!(
                    "transformer tank {name}: fewer than two ends; kept untyped"
                ));
                self.keep_untyped(tank, "fewer than two ends");
                continue;
            }

            let mut windings = Vec::new();
            let mut phases = 1usize;
            let mut ok = true;
            for (num, end) in &ends {
                let get = |key: &str| end.get(key).and_then(Value::as_f64);
                let node = end
                    .get("ConductingEquipment.Terminals")
                    .and_then(Value::as_array)
                    .and_then(|items| items.first())
                    .and_then(Value::as_object)
                    .and_then(|t| t.get("Terminal.ConnectivityNode"))
                    .and_then(pointer_name)
                    .map(str::to_owned);
                let Some(node) = node else {
                    self.warn(format!(
                        "transformer tank {name}: end {num} has no terminal; kept untyped"
                    ));
                    ok = false;
                    break;
                };
                // Both vintages of the phase attribute appear in the wild.
                let letters = end
                    .get("TransformerTankEnd.orderedPhases")
                    .or_else(|| end.get("TransformerTankEnd.phases"))
                    .and_then(Value::as_str)
                    .map(super::expand_phase_code)
                    .unwrap_or_default();
                let Some(einfo) = end_infos.get(num) else {
                    self.warn(format!(
                        "transformer tank {name}: end {num} has no TransformerEndInfo; \
                         kept untyped"
                    ));
                    ok = false;
                    break;
                };
                let iget = |key: &str| einfo.get(key).and_then(Value::as_f64);
                let conn = einfo
                    .get("TransformerEndInfo.connectionKind")
                    .and_then(Value::as_str)
                    .map_or("Y", kind_suffix);
                let conn = match conn {
                    "D" | "A" => WindingConn::Delta,
                    "Y" | "Yn" | "I" => WindingConn::Wye,
                    other => {
                        self.warn(format!(
                            "transformer tank {name}: end {num} connection `{other}` is not \
                             typed; treated as wye"
                        ));
                        WindingConn::Wye
                    }
                };
                if let Some(clock) = einfo
                    .get("TransformerEndInfo.phaseAngleClock")
                    .and_then(Value::as_u64)
                {
                    if clock != 0 {
                        self.warn(format!(
                            "transformer tank {name}: end {num} phaseAngleClock {clock} is \
                             not modeled (the model carries no vector-group shift)"
                        ));
                    }
                }

                let mut map = letters_to_terminals(&letters);
                if map.is_empty() {
                    map = vec!["1".to_owned(), "2".to_owned(), "3".to_owned()];
                    self.defaulted("transformer", &name, "phases");
                }
                let grounded = fbool_map(end, "TransformerEnd.grounded").unwrap_or(false);
                if conn == WindingConn::Wye && grounded && !map.contains(&"4".to_owned()) {
                    map.push("4".to_owned());
                }
                let hot = map.iter().filter(|t| *t != "4").count();
                phases = phases.max(hot);

                let v_ref = iget("TransformerEndInfo.ratedU").unwrap_or(0.0);
                let s_rating = iget("TransformerEndInfo.ratedS").unwrap_or(0.0);
                let mut winding = Winding::new(node.clone(), map.clone(), conn, v_ref, s_rating);
                let z_base = crate::model::n_winding_impedance_base(1, v_ref, s_rating);
                if let (Some(r), Some(z_base)) = (iget("TransformerEndInfo.r"), z_base) {
                    winding.r_pct = r / z_base * 100.0;
                }
                let (rg, xg) = (
                    get("TransformerEnd.rground").unwrap_or(0.0),
                    get("TransformerEnd.xground").unwrap_or(0.0),
                );
                if conn == WindingConn::Wye {
                    if grounded {
                        // Solid grounding is the dss default; only a real
                        // impedance needs the explicit neutral fields.
                        if rg.abs() > 0.0 || xg.abs() > 0.0 {
                            winding.r_neutral = Some(rg);
                            winding.x_neutral = Some(xg);
                        }
                        if map.contains(&"4".to_owned()) {
                            self.ground_node(&node);
                        }
                    } else {
                        // The OpenDSS ungrounded-wye convention: a negative
                        // neutral resistance opens the neutral.
                        winding.r_neutral = Some(-1.0);
                        winding.x_neutral = Some(0.0);
                        self.defaulted("transformer", &name, "rneut");
                    }
                }
                self.use_terminals(&node, &map);
                windings.push(winding);
            }
            if !ok {
                self.keep_untyped(tank, "incomplete ends");
                continue;
            }

            // Short-circuit reactance: the test rides inline under the
            // energised end's catalog record; the pairing beyond two
            // windings is not encoded, so it maps for two-winding tanks.
            let n_pairs = crate::model::pair_keys(windings.len()).len();
            let mut xsc_pct = vec![0.0; n_pairs];
            let mut found_sc = false;
            for (num, einfo) in &end_infos {
                let Some(Value::Array(tests)) =
                    einfo.get("TransformerEndInfo.EnergisedEndShortCircuitTests")
                else {
                    continue;
                };
                for test in tests {
                    let Some(test) = test.as_object() else {
                        continue;
                    };
                    let z = test
                        .get("ShortCircuitTest.leakageImpedance")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    let base_power = test
                        .get("TransformerTest.basePower")
                        .and_then(Value::as_f64)
                        .or_else(|| {
                            einfo
                                .get("TransformerEndInfo.ratedS")
                                .and_then(Value::as_f64)
                        })
                        .unwrap_or(0.0);
                    let rated_u = einfo
                        .get("TransformerEndInfo.ratedU")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    if let Some(z_base) =
                        crate::model::n_winding_impedance_base(1, rated_u, base_power)
                    {
                        if !found_sc {
                            xsc_pct[0] = z / z_base * 100.0;
                            found_sc = true;
                        } else if windings.len() > 2 {
                            self.warn(format!(
                                "transformer tank {name}: additional short-circuit test on \
                                 end {num} is not mapped (winding-pair topology beyond two \
                                 windings is not encoded in the document)"
                            ));
                        }
                    }
                }
            }
            if !found_sc {
                self.defaulted("transformer", &name, "xhl");
            }

            // The model's mRID is the enclosing `PowerTransformer`'s (matched
            // by name, the upstream convention), not the tank's: the writer
            // regenerates the tank, catalog, and test ids deterministically
            // from the transformer name, and applies the imported id to the
            // `PowerTransformer` object.
            let mut extras = self
                .doc
                .of_kind("PowerTransformer")
                .find(|p| p.name == name)
                .map_or_else(Extras::new, extras_with_mrid);
            // No-load behavior: percent values on the winding-1 base, the
            // dss `%noloadloss` / `%imag` properties.
            for einfo in end_infos.values() {
                let Some(Value::Array(tests)) =
                    einfo.get("TransformerEndInfo.EnergisedEndNoLoadTests")
                else {
                    continue;
                };
                for test in tests {
                    let Some(test) = test.as_object() else {
                        continue;
                    };
                    let base_power = test
                        .get("TransformerTest.basePower")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    if let Some(loss) = test.get("NoLoadTest.loss").and_then(Value::as_f64) {
                        if base_power > 0.0 && loss.abs() > 0.0 {
                            extras.insert("%noloadloss".into(), (loss / base_power * 100.0).into());
                        }
                    }
                    if let Some(current) = test
                        .get("NoLoadTest.excitingCurrent")
                        .and_then(Value::as_f64)
                    {
                        if current.abs() > 0.0 {
                            extras.insert("%imag".into(), current.into());
                        }
                    }
                }
                break;
            }

            self.apply_tap_changers(&name, &mut windings);
            let mut transformer = DistTransformer::new(name, windings, xsc_pct, phases.max(1));
            transformer.extras = extras;
            self.net.transformers.push(transformer);
        }
    }

    /// `RatioTapChanger` records link to their tank by name in the upstream
    /// exports (no end pointer); the regulated end is matched through
    /// `TapChanger.neutralU` against the winding rated voltages. The control
    /// settings themselves stay untyped.
    fn apply_tap_changers(&mut self, tank: &str, windings: &mut [Winding]) {
        let Some(rtc) = self.doc.of_kind("RatioTapChanger").find(|o| o.name == tank) else {
            return;
        };
        let step = f_or(rtc, "TapChanger.step", 1.0);
        let neutral_step = f_or(rtc, "TapChanger.neutralStep", 0.0);
        let increment = f_or(rtc, "RatioTapChanger.stepVoltageIncrement", 0.0);
        // Two vintages: `step` as the tap ratio directly (the OpenDSS
        // exporters), or as an integer position against neutralStep and a
        // percent increment (the CIM definition).
        let ratio = if (0.5..=1.5).contains(&step) {
            step
        } else {
            1.0 + (step - neutral_step) * increment / 100.0
        };
        let neutral_u = f_or(rtc, "TapChanger.neutralU", 0.0);
        let target = windings
            .iter()
            .position(|w| neutral_u > 0.0 && (w.v_ref - neutral_u).abs() <= 1e-6 * neutral_u.abs())
            .or_else(|| windings.len().checked_sub(1));
        if let Some(i) = target {
            windings[i].tap = ratio;
        }
        self.warn(format!(
            "transformer tank {tank}: RatioTapChanger sets the winding tap to {ratio}; the \
             regulator control settings are preserved untyped"
        ));
    }

    // ----- buses ------------------------------------------------------------

    fn finish_buses(&mut self) {
        self.claim(&["Location", "PositionPoint"]);
        let mut locations: BTreeMap<String, crate::geo::Location> = BTreeMap::new();
        for obj in self.doc.of_kind("Location") {
            let Some(node) = obj.name.strip_suffix("_Location") else {
                continue;
            };
            let point = obj
                .props
                .get("Location.PositionPoints")
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(Value::as_object);
            let coord = |key: &str| {
                point.and_then(|p| p.get(key)).and_then(|v| {
                    v.as_f64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
            };
            if let (Some(x), Some(y)) = (
                coord("PositionPoint.xPosition"),
                coord("PositionPoint.yPosition"),
            ) {
                locations.insert(node.to_owned(), crate::geo::Location { x, y, kind: None });
            }
        }

        for obj in self.doc.sorted_of_kind("ConnectivityNode") {
            let terminals: Vec<String> = self
                .node_terminals
                .get(&obj.name)
                .map(|set| set.iter().map(u8::to_string).collect())
                .unwrap_or_default();
            let mut bus = DistBus::new(obj.name.clone(), terminals);
            if self.node_grounded.contains(&obj.name) {
                bus.grounded = vec!["4".to_owned()];
            }
            if let Some(set) = obj
                .props
                .get("ConnectivityNode.OperationalLimitSet")
                .and_then(pointer_name)
            {
                let limits = limit_set(self.doc, set);
                bus.v_min = limits.v_low;
                bus.v_max = limits.v_high;
            }
            bus.location = locations.remove(&obj.name);
            bus.extras = extras_with_mrid(obj);
            self.net.buses.push(bus);
        }

        // Terminals seen on nodes the document never declared.
        let declared: BTreeSet<&String> = self.net.buses.iter().map(|b| &b.id).collect();
        let missing: Vec<String> = self
            .node_terminals
            .keys()
            .filter(|n| !declared.contains(n))
            .cloned()
            .collect();
        for node in missing {
            self.warn(format!(
                "terminal points at undeclared ConnectivityNode `{node}`; bus created"
            ));
            let terminals: Vec<String> = self.node_terminals[&node]
                .iter()
                .map(u8::to_string)
                .collect();
            let mut bus = DistBus::new(node.clone(), terminals);
            if self.node_grounded.contains(&node) {
                bus.grounded = vec!["4".to_owned()];
            }
            self.net.buses.push(bus);
        }
    }

    // ----- untyped preservation ----------------------------------------------

    fn keep_untyped(&mut self, obj: &Obj, why: &str) {
        self.warn(format!(
            "{} {}: {why}; preserved untyped (round-trips to MG-RAVENS, dropped elsewhere)",
            obj.kind, obj.name
        ));
        self.push_untyped(obj);
    }

    fn push_untyped(&mut self, obj: &Obj) {
        let raw = serde_json::to_string(&Value::Object(obj.props.clone())).unwrap_or_default();
        self.net.untyped.push(UntypedObject::new(
            obj.kind.clone(),
            obj.name.clone(),
            vec![
                (Some("ravens_path".into()), obj.path.join("/")),
                (Some("ravens_json".into()), raw),
            ],
        ));
    }

    /// Kinds with no typed mapping are preserved per object (with their root
    /// path, so the writer re-nests them exactly), and reported per kind.
    fn preserve_unclaimed(&mut self) {
        let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
        let unclaimed: Vec<usize> = self
            .doc
            .objects
            .iter()
            .enumerate()
            .filter(|(_, o)| !o.nested && !self.consumed.contains(o.kind.as_str()))
            .map(|(i, _)| i)
            .collect();
        for i in unclaimed {
            let obj = &self.doc.objects[i];
            *by_kind.entry(obj.kind.clone()).or_default() += 1;
            self.push_untyped(obj);
        }
        for (kind, count) in by_kind {
            self.warn(format!(
                "{count} {kind} object(s) have no typed model; preserved untyped \
                 (round-trips to MG-RAVENS, dropped elsewhere)"
            ));
        }
    }
}

fn fbool_map(map: &Map<String, Value>, key: &str) -> Option<bool> {
    map.get(key).and_then(Value::as_bool)
}

/// A fresh extras map carrying the object's imported mRID under
/// [`MRID_KEY`], so a RAVENS round trip reuses the source id.
fn extras_with_mrid(obj: &Obj) -> Extras {
    let mut extras = Extras::new();
    if let Some(id) = mrid(obj) {
        extras.insert(MRID_KEY.into(), id.into());
    }
    extras
}
