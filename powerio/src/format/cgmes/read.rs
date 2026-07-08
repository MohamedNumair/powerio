//! Merged CGMES profiles into a bus-branch [`Network`].
//!
//! `TopologicalNode` is the bus. Terminals tie conducting equipment to nodes
//! (directly in 2.4.15 TP; through `ConnectivityNode.TopologicalNode` in
//! 3.0). SSH carries the operating point (`p`/`q`, switch state, tap steps,
//! sections); SV the solved state (`SvVoltage`, `SvTapStep`, `SvPowerFlow`).
//! Missing SSH degrades gracefully — vendor exports like the CIGRE MV set
//! ship EQ/TP/SV only, so element `p`/`q` falls back to the terminal's
//! `SvPowerFlow`.
//!
//! CGMES values are MW/MVAr/kV/ohm/S; per-unit lands on a 100 MVA system
//! base. Everything the mapping does not consume is counted per class into
//! the parse warnings, never dropped silently.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::CgmesVersion;
use super::xml::{CimDocument, CimObject, ModelHeader, PropValue, parse_cimxml};
use crate::format::Parsed;
use crate::network::{
    Branch, BranchCharging, Bus, BusId, BusType, Generator, Load, Network, Shunt, SourceFormat,
    Switch,
};
use crate::{Error, Result};

const FMT: &str = "CGMES";
/// CGMES has no system MVA base; every per-unit value lands on this one.
const SYSTEM_MVA: f64 = 100.0;

/// Cheap directory sniff: any `.xml` whose head looks like a CIM RDF file.
pub(crate) fn dir_has_cgmes(dir: &Path) -> bool {
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
        let head = &text[..text.len().min(4096)];
        if head.contains("rdf-syntax") && CgmesVersion::from_namespace(head).is_some() {
            return true;
        }
    }
    false
}

/// Read every CGMES instance file under `dir` (non-recursive) as one case.
///
/// # Errors
/// [`Error::FormatRead`] when the directory holds no CGMES files, no
/// `TopologicalNode` records (the bus-branch view needs a TP part), or the
/// XML is malformed; [`Error::Io`] on unreadable files.
pub fn read_cgmes_dir(dir: impl AsRef<Path>) -> Result<Parsed> {
    let dir = dir.as_ref();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("xml"))
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Err(Error::FormatRead {
            format: FMT,
            message: format!("no .xml instance files in {}", dir.display()),
        });
    }
    let stem = dir.file_name().and_then(|s| s.to_str());
    read_cgmes_paths(&paths, stem)
}

/// The per-file role, from the `md:FullModel` profile URIs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    Model,
    /// Diagram layout / geography / dynamics: valid parts of a set, but
    /// nothing the balanced model consumes; counted, not merged.
    Presentation,
}

fn classify(header: Option<&ModelHeader>) -> Profile {
    let Some(header) = header else {
        return Profile::Model;
    };
    let presentation = header.profiles.iter().all(|p| {
        p.contains("DiagramLayout") || p.contains("GeographicalLocation") || p.contains("Dynamics")
    });
    if presentation && !header.profiles.is_empty() {
        Profile::Presentation
    } else {
        Profile::Model
    }
}

/// One object merged across the profile files.
struct Merged {
    id: String,
    class: String,
    props: Vec<(String, PropValue)>,
}

/// The merged object store plus the id and class indexes the mapping reads.
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

    /// Ids of every object of `class`, in first-definition order.
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

    /// A reference property's target id.
    fn refv(&self, id: &str, key: &str) -> Option<&str> {
        match self.prop(id, key)? {
            PropValue::Ref(target) => Some(target),
            PropValue::Text(_) => None,
        }
    }

    /// The `value` half of an `EnumClass.value` reference.
    fn enum_value(&self, id: &str, key: &str) -> Option<&str> {
        self.refv(id, key)?.rsplit('.').next()
    }

    fn name(&self, id: &str) -> String {
        self.text(id, "IdentifiedObject.name")
            .map_or_else(|| id.to_string(), str::to_string)
    }
}

/// Terminal wiring: equipment → its terminals (sequence order), terminal →
/// topological node, terminal SSH connected state.
struct Wiring {
    of_equipment: HashMap<String, Vec<String>>,
    node_of: HashMap<String, String>,
    connected: HashMap<String, bool>,
}

impl Wiring {
    fn build(store: &Store) -> Wiring {
        let mut of_equipment: HashMap<String, Vec<(f64, String)>> = HashMap::new();
        let mut node_of = HashMap::new();
        let mut connected = HashMap::new();
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
            // 2.4.15 TP links the terminal itself; 3.0 links through the
            // connectivity node. Either way the terminal lands on a TN.
            let tn = store.refv(id, "Terminal.TopologicalNode").or_else(|| {
                let cn = store.refv(id, "Terminal.ConnectivityNode")?;
                store.refv(cn, "ConnectivityNode.TopologicalNode")
            });
            if let Some(tn) = tn {
                node_of.insert(id.to_string(), tn.to_string());
            }
            if let Some(state) = store.boolean(id, "ACDCTerminal.connected") {
                connected.insert(id.to_string(), state);
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
            connected,
        }
    }

    fn terminals(&self, equipment: &str) -> &[String] {
        self.of_equipment.get(equipment).map_or(&[], Vec::as_slice)
    }

    fn node(&self, terminal: &str) -> Option<&str> {
        self.node_of.get(terminal).map(String::as_str)
    }

    /// In service as far as the terminals say: every terminal connected
    /// (missing SSH data reads as connected).
    fn energized(&self, equipment: &str) -> bool {
        self.terminals(equipment)
            .iter()
            .all(|t| self.connected.get(t).copied().unwrap_or(true))
    }
}

/// Everything the element builders share.
struct Mapper<'a> {
    store: &'a Store,
    wiring: Wiring,
    bus_of_tn: HashMap<String, BusId>,
    kv_of: HashMap<BusId, f64>,
    /// Terminal → solved SvPowerFlow (p, q), the SSH fallback.
    sv_flow: HashMap<String, (f64, f64)>,
    warnings: &'a mut Vec<String>,
}

impl Mapper<'_> {
    fn bus_of_equipment_terminal(&mut self, equipment: &str, index: usize) -> Option<BusId> {
        let terminal = self.wiring.terminals(equipment).get(index)?.clone();
        let tn = self.wiring.node(&terminal)?.to_string();
        self.bus_of_tn.get(tn.as_str()).copied()
    }

    fn kv(&self, bus: BusId) -> f64 {
        let kv = self.kv_of.get(&bus).copied().unwrap_or(0.0);
        if kv > 0.0 { kv } else { 1.0 }
    }

    /// SSH value with SvPowerFlow fallback (vendor sets without SSH).
    fn power(&self, equipment: &str, key: &str) -> (f64, f64) {
        let store = self.store;
        if let (Some(p), Some(q)) = (
            store.f(equipment, &format!("{key}.p")),
            store.f(equipment, &format!("{key}.q")),
        ) {
            return (p, q);
        }
        for terminal in self.wiring.terminals(equipment) {
            if let Some(&(p, q)) = self.sv_flow.get(terminal) {
                return (p, q);
            }
        }
        (0.0, 0.0)
    }

    fn in_service(&self, equipment: &str) -> bool {
        self.store
            .boolean(equipment, "Equipment.inService")
            .unwrap_or_else(|| self.wiring.energized(equipment))
    }
}

/// Read an explicit CGMES file set as one case.
pub(crate) fn read_cgmes_paths(paths: &[PathBuf], name_hint: Option<&str>) -> Result<Parsed> {
    let mut warnings = Vec::new();
    let mut store = Store {
        objects: Vec::new(),
        by_id: HashMap::new(),
    };
    let mut versions: Vec<CgmesVersion> = Vec::new();
    let mut description: Option<String> = None;
    let mut skipped: Vec<String> = Vec::new();

    for path in paths {
        let text = std::fs::read_to_string(path)?;
        let doc = parse_cimxml(&text)?;
        if let Some(ns) = &doc.cim_namespace {
            if let Some(version) = CgmesVersion::from_namespace(ns) {
                if !versions.contains(&version) {
                    versions.push(version);
                }
            }
        }
        if classify(doc.header.as_ref()) == Profile::Presentation {
            skipped.push(path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ));
            continue;
        }
        if description.is_none() {
            description = doc.header.as_ref().and_then(|h| h.description.clone());
        }
        store.merge(doc);
    }

    let version = match versions.as_slice() {
        [] => {
            return Err(Error::FormatRead {
                format: FMT,
                message: "no file declares a CIM16/CIM100 namespace; not a CGMES set".into(),
            });
        }
        [one] => *one,
        many => {
            warnings.push(format!(
                "mixed CGMES versions in one set ({}); reading with {} semantics",
                many.iter()
                    .map(|v| v.label())
                    .collect::<Vec<_>>()
                    .join(", "),
                many[0].label()
            ));
            many[0]
        }
    };
    if !skipped.is_empty() {
        warnings.push(format!(
            "presentation/dynamics parts not mapped: {}",
            skipped.join(", ")
        ));
    }

    let network = build(&store, version, description, name_hint, &mut warnings)?;
    Ok(Parsed { network, warnings })
}

/// Switch classes, all mapping to the neutral switch record.
const SWITCH_CLASSES: [&str; 8] = [
    "Breaker",
    "Disconnector",
    "LoadBreakSwitch",
    "Switch",
    "Fuse",
    "Jumper",
    "GroundDisconnector",
    "DisconnectingCircuitBreaker",
];

/// Load classes sharing the EnergyConsumer attribute set.
const LOAD_CLASSES: [&str; 4] = [
    "EnergyConsumer",
    "ConformLoad",
    "NonConformLoad",
    "StationSupply",
];

/// Classes the mapping consumes structurally (no "unmapped" warning).
const CONSUMED: [&str; 25] = [
    "TopologicalNode",
    "ConnectivityNode",
    "Terminal",
    "BaseVoltage",
    "BaseFrequency",
    "SvVoltage",
    "SvPowerFlow",
    "SvTapStep",
    "SvShuntCompensatorSections",
    "TopologicalIsland",
    "ACLineSegment",
    "PowerTransformer",
    "PowerTransformerEnd",
    "RatioTapChanger",
    "PhaseTapChangerLinear",
    "SynchronousMachine",
    "GeneratingUnit",
    "ThermalGeneratingUnit",
    "HydroGeneratingUnit",
    "WindGeneratingUnit",
    "SolarGeneratingUnit",
    "NuclearGeneratingUnit",
    "ExternalNetworkInjection",
    "LinearShuntCompensator",
    "EquivalentInjection",
];

#[allow(clippy::too_many_lines)] // the element families map in one ordered pass
fn build(
    store: &Store,
    version: CgmesVersion,
    description: Option<String>,
    name_hint: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<Network> {
    let wiring = Wiring::build(store);

    // Buses from TopologicalNode, ids in definition order.
    let mut buses = Vec::new();
    let mut bus_of_tn = HashMap::new();
    let mut kv_of = HashMap::new();
    for (i, tn) in store.of_class("TopologicalNode").enumerate() {
        let id = BusId::new(i + 1);
        let kv = store
            .refv(tn, "TopologicalNode.BaseVoltage")
            .and_then(|bv| store.f(bv, "BaseVoltage.nominalVoltage"))
            .unwrap_or(0.0);
        let mut bus = Bus::new(id, BusType::Pq, kv);
        bus.name = Some(store.name(tn));
        bus.uid = Some(tn.to_string());
        buses.push(bus);
        bus_of_tn.insert(tn.to_string(), id);
        kv_of.insert(id, kv);
    }
    if buses.is_empty() {
        return Err(Error::FormatRead {
            format: FMT,
            message: "no TopologicalNode records; the bus-branch reader needs the TP \
                      part of the set (node-breaker collapse is follow-up work)"
                .into(),
        });
    }

    // Solved state onto the buses.
    for sv in store.of_class("SvVoltage") {
        let Some(bus) = store
            .refv(sv, "SvVoltage.TopologicalNode")
            .and_then(|tn| bus_of_tn.get(tn))
        else {
            continue;
        };
        let bus = &mut buses[bus.0 - 1];
        if let Some(v) = store.f(sv, "SvVoltage.v") {
            let kv = if bus.base_kv > 0.0 { bus.base_kv } else { 1.0 };
            bus.vm = v / kv;
        }
        if let Some(angle) = store.f(sv, "SvVoltage.angle") {
            bus.va = angle;
        }
    }
    let mut sv_flow = HashMap::new();
    for sv in store.of_class("SvPowerFlow") {
        if let (Some(terminal), Some(p), Some(q)) = (
            store.refv(sv, "SvPowerFlow.Terminal"),
            store.f(sv, "SvPowerFlow.p"),
            store.f(sv, "SvPowerFlow.q"),
        ) {
            sv_flow.insert(terminal.to_string(), (p, q));
        }
    }

    let mut mapper = Mapper {
        store,
        wiring,
        bus_of_tn,
        kv_of,
        sv_flow,
        warnings,
    };

    let mut loads = read_loads(&mut mapper);
    let (mut generators, ref_candidates) = read_machines(&mut mapper);
    let shunts = read_shunts(&mut mapper);
    let branches = read_branches(&mut mapper, version);
    let switches = read_switches(&mut mapper);
    read_equivalent_injections(&mut mapper, &mut loads, &mut generators);

    // Reference bus: the island's angle reference, else the best candidate
    // (lowest referencePriority, then an external injection, then the
    // largest machine).
    let mut reference: Option<BusId> = None;
    for island in store.of_class("TopologicalIsland") {
        if let Some(bus) = store
            .refv(island, "TopologicalIsland.AngleRefTopologicalNode")
            .and_then(|tn| mapper.bus_of_tn.get(tn))
        {
            reference = Some(*bus);
            break;
        }
    }
    let reference = reference.or(ref_candidates);
    match reference {
        Some(id) => buses[id.0 - 1].kind = BusType::Ref,
        None => mapper.warnings.push(
            "no angle reference in the set (no TopologicalIsland, reference \
             priority, or external injection); matrix consumers will report \
             the missing slack"
                .into(),
        ),
    }
    for generator in &generators {
        let bus = &mut buses[generator.bus.0 - 1];
        if bus.kind == BusType::Pq {
            bus.kind = BusType::Pv;
        }
    }

    warn_unmapped(store, mapper.warnings);

    let base_frequency = store
        .of_class("BaseFrequency")
        .next()
        .and_then(|f| store.f(f, "BaseFrequency.frequency"))
        .unwrap_or_else(|| {
            mapper
                .warnings
                .push("no BaseFrequency record; assuming 50 Hz".into());
            50.0
        });
    mapper.warnings.push(format!(
        "{}: per-unit values normalized onto a 100 MVA system base (CGMES \
         carries none)",
        version.label()
    ));

    let name = description
        .filter(|d| !d.is_empty())
        .or_else(|| name_hint.map(str::to_string))
        .unwrap_or_else(|| "cgmes case".into());
    let mut net = Network::new(name, SYSTEM_MVA);
    net.base_frequency = base_frequency;
    net.buses = buses;
    net.loads = loads;
    net.shunts = shunts;
    net.branches = branches;
    net.switches = switches;
    net.generators = generators;
    net.source_format = SourceFormat::Cgmes;
    Ok(net)
}

fn read_loads(mapper: &mut Mapper<'_>) -> Vec<Load> {
    let mut loads = Vec::new();
    for class in LOAD_CLASSES {
        for id in mapper.store.of_class(class) {
            let Some(bus) = mapper.bus_of_equipment_terminal(id, 0) else {
                mapper.warnings.push(format!(
                    "{class} {}: no terminal on a topological node; skipped",
                    mapper.store.name(id)
                ));
                continue;
            };
            let (p, q) = mapper.power(id, "EnergyConsumer");
            let mut load = Load::new(bus, p, q);
            load.in_service = mapper.in_service(id);
            load.uid = Some(id.to_string());
            loads.push(load);
        }
    }
    loads
}

fn read_machines(mapper: &mut Mapper<'_>) -> (Vec<Generator>, Option<BusId>) {
    let mut generators = Vec::new();
    let mut best: Option<(f64, BusId)> = None;
    let mut external: Option<BusId> = None;
    let mut largest: Option<(f64, BusId)> = None;
    for id in mapper.store.of_class("SynchronousMachine") {
        let Some(bus) = mapper.bus_of_equipment_terminal(id, 0) else {
            mapper.warnings.push(format!(
                "SynchronousMachine {}: no terminal on a topological node; skipped",
                mapper.store.name(id)
            ));
            continue;
        };
        let store = mapper.store;
        let (p, q) = mapper.power(id, "RotatingMachine");
        let mut generator = Generator::new(bus);
        generator.pg = -p;
        generator.qg = -q;
        generator.qmax = store.f(id, "SynchronousMachine.maxQ").unwrap_or(0.0);
        generator.qmin = store.f(id, "SynchronousMachine.minQ").unwrap_or(0.0);
        generator.mbase = store.f(id, "RotatingMachine.ratedS").unwrap_or(0.0);
        generator.in_service = mapper.in_service(id);
        generator.uid = Some(id.to_string());
        if let Some(unit) = store.refv(id, "RotatingMachine.GeneratingUnit") {
            generator.pmin = store.f(unit, "GeneratingUnit.minOperatingP").unwrap_or(0.0);
            generator.pmax = store.f(unit, "GeneratingUnit.maxOperatingP").unwrap_or(0.0);
        }
        apply_regulation(mapper, id, &mut generator);
        if let Some(priority) = mapper.store.f(id, "SynchronousMachine.referencePriority") {
            if priority > 0.0 && best.is_none_or(|(p0, _)| priority < p0) {
                best = Some((priority, bus));
            }
        }
        if largest.is_none_or(|(s, _)| generator.mbase > s) {
            largest = Some((generator.mbase, bus));
        }
        generators.push(generator);
    }
    for id in mapper.store.of_class("ExternalNetworkInjection") {
        let Some(bus) = mapper.bus_of_equipment_terminal(id, 0) else {
            continue;
        };
        let store = mapper.store;
        let (p, q) = mapper.power(id, "ExternalNetworkInjection");
        let mut generator = Generator::new(bus);
        generator.pg = -p;
        generator.qg = -q;
        generator.pmax = store.f(id, "ExternalNetworkInjection.maxP").unwrap_or(0.0);
        generator.pmin = store.f(id, "ExternalNetworkInjection.minP").unwrap_or(0.0);
        generator.qmax = store.f(id, "ExternalNetworkInjection.maxQ").unwrap_or(0.0);
        generator.qmin = store.f(id, "ExternalNetworkInjection.minQ").unwrap_or(0.0);
        generator.in_service = mapper.in_service(id);
        generator.uid = Some(id.to_string());
        apply_regulation(mapper, id, &mut generator);
        if external.is_none() {
            external = Some(bus);
        }
        generators.push(generator);
    }
    let reference = best
        .map(|(_, bus)| bus)
        .or(external)
        .or(largest.map(|(_, b)| b));
    (generators, reference)
}

/// Voltage-mode `RegulatingControl` → `vg` (target over the regulated node's
/// base) and the remote regulated bus when it is not the machine's own.
fn apply_regulation(mapper: &mut Mapper<'_>, machine: &str, generator: &mut Generator) {
    let store = mapper.store;
    let Some(control) = store.refv(machine, "RegulatingCondEq.RegulatingControl") else {
        return;
    };
    if mapper.store.enum_value(control, "RegulatingControl.mode") != Some("voltage") {
        return;
    }
    let Some(target) = store.f(control, "RegulatingControl.targetValue") else {
        return;
    };
    let regulated = store
        .refv(control, "RegulatingControl.Terminal")
        .and_then(|t| mapper.wiring.node(t))
        .and_then(|tn| mapper.bus_of_tn.get(tn))
        .copied();
    if let Some(bus) = regulated {
        let kv = mapper.kv(bus);
        if target > 0.0 {
            generator.vg = target / kv;
        }
        if bus != generator.bus {
            generator.regulated_bus = Some(bus);
        }
    }
}

fn read_shunts(mapper: &mut Mapper<'_>) -> Vec<Shunt> {
    let mut shunts = Vec::new();
    for id in mapper.store.of_class("LinearShuntCompensator") {
        let Some(bus) = mapper.bus_of_equipment_terminal(id, 0) else {
            continue;
        };
        let store = mapper.store;
        let kv = mapper.kv(bus);
        let sections = store
            .f(id, "ShuntCompensator.sections")
            .or_else(|| sv_sections(store, id))
            .or_else(|| store.f(id, "ShuntCompensator.normalSections"))
            .unwrap_or(1.0);
        // Siemens × kV² = MW/MVAr injected at nominal voltage, the model's
        // 1 p.u. convention.
        let g = store
            .f(id, "LinearShuntCompensator.gPerSection")
            .unwrap_or(0.0)
            * sections
            * (kv * kv);
        let b = store
            .f(id, "LinearShuntCompensator.bPerSection")
            .unwrap_or(0.0)
            * sections
            * (kv * kv);
        let mut shunt = Shunt::new(bus, g, b);
        shunt.in_service = mapper.in_service(id) && sections > 0.0;
        shunt.uid = Some(id.to_string());
        shunts.push(shunt);
    }
    let nonlinear = mapper.store.of_class("NonlinearShuntCompensator").count();
    if nonlinear > 0 {
        mapper.warnings.push(format!(
            "{nonlinear} NonlinearShuntCompensator(s) are not mapped (per-section \
             point tables need a richer shunt model)"
        ));
    }
    shunts
}

fn sv_sections(store: &Store, shunt: &str) -> Option<f64> {
    store.of_class("SvShuntCompensatorSections").find_map(|sv| {
        (store.refv(sv, "SvShuntCompensatorSections.ShuntCompensator") == Some(shunt))
            .then(|| store.f(sv, "SvShuntCompensatorSections.sections"))
            .flatten()
    })
}

fn read_switches(mapper: &mut Mapper<'_>) -> Vec<Switch> {
    let mut switches = Vec::new();
    let mut internal = 0usize;
    for class in SWITCH_CLASSES {
        for id in mapper.store.of_class(class) {
            let (Some(from), Some(to)) = (
                mapper.bus_of_equipment_terminal(id, 0),
                mapper.bus_of_equipment_terminal(id, 1),
            ) else {
                continue;
            };
            if from == to {
                // Closed inside one topological node: already collapsed by
                // the topology processor that produced TP.
                internal += 1;
                continue;
            }
            let store = mapper.store;
            let open = store
                .boolean(id, "Switch.open")
                .or_else(|| store.boolean(id, "Switch.normalOpen"))
                .unwrap_or(false);
            let mut switch = Switch::new(from, to, !open);
            switch.current_rating = store.f(id, "Switch.ratedCurrent");
            switch.uid = Some(id.to_string());
            switches.push(switch);
        }
    }
    if internal > 0 {
        mapper.warnings.push(format!(
            "{internal} switch(es) internal to one topological node are represented \
             by the topology itself"
        ));
    }
    switches
}

/// Boundary-point injections become loads: they model the neighboring
/// network's net demand at the tie node.
fn read_equivalent_injections(
    mapper: &mut Mapper<'_>,
    loads: &mut Vec<Load>,
    generators: &mut Vec<Generator>,
) {
    let mut count = 0usize;
    for id in mapper.store.of_class("EquivalentInjection") {
        let Some(bus) = mapper.bus_of_equipment_terminal(id, 0) else {
            continue;
        };
        let (p, q) = mapper.power(id, "EquivalentInjection");
        let regulation = mapper
            .store
            .boolean(id, "EquivalentInjection.regulationStatus")
            .unwrap_or(false);
        if regulation {
            let mut generator = Generator::new(bus);
            generator.pg = -p;
            generator.qg = -q;
            generator.uid = Some(id.to_string());
            generators.push(generator);
        } else {
            let mut load = Load::new(bus, p, q);
            load.in_service = mapper.in_service(id);
            load.uid = Some(id.to_string());
            loads.push(load);
        }
        count += 1;
    }
    if count > 0 {
        mapper.warnings.push(format!(
            "{count} EquivalentInjection(s) at boundary nodes mapped to \
             loads/generators (p/q at the tie point)"
        ));
    }
}

fn read_branches(mapper: &mut Mapper<'_>, version: CgmesVersion) -> Vec<Branch> {
    let mut branches = Vec::new();
    for id in mapper.store.of_class("ACLineSegment") {
        let (Some(from), Some(to)) = (
            mapper.bus_of_equipment_terminal(id, 0),
            mapper.bus_of_equipment_terminal(id, 1),
        ) else {
            mapper.warnings.push(format!(
                "ACLineSegment {}: terminals do not land on two topological \
                 nodes (boundary line without its boundary set?); skipped",
                mapper.store.name(id)
            ));
            continue;
        };
        let store = mapper.store;
        let kv = mapper.kv(from);
        let z_base = kv * kv / SYSTEM_MVA;
        let y_base = SYSTEM_MVA / (kv * kv);
        let mut branch = Branch::new(
            from,
            to,
            store.f(id, "ACLineSegment.r").unwrap_or(0.0) / z_base,
            store.f(id, "ACLineSegment.x").unwrap_or(0.0) / z_base,
        );
        branch.b = store.f(id, "ACLineSegment.bch").unwrap_or(0.0) / y_base;
        let g = store.f(id, "ACLineSegment.gch").unwrap_or(0.0) / y_base;
        if g != 0.0 {
            let half_b = branch.b / 2.0;
            branch.charging = Some(BranchCharging::new(g / 2.0, half_b, g / 2.0, half_b));
        }
        branch.in_service = mapper.in_service(id);
        branch.uid = Some(id.to_string());
        apply_limits(mapper, id, &mut branch, kv, version);
        branches.push(branch);
    }
    read_transformers(mapper, &mut branches, version);
    let series = mapper.store.of_class("SeriesCompensator").count();
    if series > 0 {
        mapper
            .warnings
            .push(format!("{series} SeriesCompensator(s) are not mapped yet"));
    }
    branches
}

fn read_transformers(mapper: &mut Mapper<'_>, branches: &mut Vec<Branch>, version: CgmesVersion) {
    // Ends grouped per transformer, ordered by endNumber.
    let mut ends_of: BTreeMap<String, Vec<(f64, String)>> = BTreeMap::new();
    for end in mapper.store.of_class("PowerTransformerEnd") {
        if let Some(xf) = mapper
            .store
            .refv(end, "PowerTransformerEnd.PowerTransformer")
        {
            let number = mapper
                .store
                .f(end, "TransformerEnd.endNumber")
                .unwrap_or(1.0);
            ends_of
                .entry(xf.to_string())
                .or_default()
                .push((number, end.to_string()));
        }
    }
    let mut three_winding = 0usize;
    for (xf, mut ends) in ends_of {
        ends.sort_by(|a, b| a.0.total_cmp(&b.0));
        if ends.len() != 2 {
            three_winding += 1;
            continue;
        }
        let store = mapper.store;
        let end_terminal = |end: &str| {
            store
                .refv(end, "TransformerEnd.Terminal")
                .and_then(|t| mapper.wiring.node(t))
                .and_then(|tn| mapper.bus_of_tn.get(tn))
                .copied()
        };
        let (end1, end2) = (ends[0].1.as_str(), ends[1].1.as_str());
        let (Some(from), Some(to)) = (end_terminal(end1), end_terminal(end2)) else {
            mapper.warnings.push(format!(
                "PowerTransformer {}: ends do not land on topological nodes; skipped",
                store.name(&xf)
            ));
            continue;
        };
        // Each end's ohms are at its own rated voltage; per-unit them on the
        // end's rated base and sum onto the series branch.
        let rated = |end: &str| store.f(end, "PowerTransformerEnd.ratedU").unwrap_or(0.0);
        let (u1, u2) = (rated(end1), rated(end2));
        let pu = |end: &str, key: &str, u: f64| {
            let u = if u > 0.0 { u } else { 1.0 };
            store.f(end, key).unwrap_or(0.0) / (u * u / SYSTEM_MVA)
        };
        let r = pu(end1, "PowerTransformerEnd.r", u1) + pu(end2, "PowerTransformerEnd.r", u2);
        let x = pu(end1, "PowerTransformerEnd.x", u1) + pu(end2, "PowerTransformerEnd.x", u2);
        let mut branch = Branch::new(from, to, r, x);
        let b_pu = |end: &str, u: f64| {
            let u = if u > 0.0 { u } else { 1.0 };
            store.f(end, "PowerTransformerEnd.b").unwrap_or(0.0) * (u * u / SYSTEM_MVA)
        };
        let (b1, b2) = (b_pu(end1, u1), b_pu(end2, u2));
        if b1 != 0.0 || b2 != 0.0 {
            branch.b = b1 + b2;
            branch.charging = Some(BranchCharging::new(0.0, b1, 0.0, b2));
        }

        // Off-nominal ratio: rated-to-bus-base on each side, times the ratio
        // tap changer's in-service step.
        let (kv1, kv2) = (mapper.kv(from), mapper.kv(to));
        let mut tap = (safe_div(u1, kv1)) / (safe_div(u2, kv2));
        for (end, invert) in [(end1, false), (end2, true)] {
            if let Some(rtc) = store.refv(end, "TransformerEnd.RatioTapChanger") {
                let factor = ratio_tap_factor(mapper, rtc);
                if invert {
                    tap /= factor;
                } else {
                    tap *= factor;
                }
            }
            if let Some(ptc) = phase_tap_changer(store, end) {
                branch.shift += phase_shift_deg(mapper, &ptc, invert);
            }
        }
        branch.tap = tap;
        branch.in_service = mapper.in_service(&xf)
            && mapper.wiring.energized(end1)
            && mapper.wiring.energized(end2);
        branch.uid = Some(xf.clone());
        apply_limits(mapper, &xf, &mut branch, kv1, version);
        for end in [end1, end2] {
            apply_limits(mapper, end, &mut branch, kv1, version);
        }
        branches.push(branch);
    }
    if three_winding > 0 {
        mapper.warnings.push(format!(
            "{three_winding} transformer(s) with other than two windings are not \
             mapped yet (3-winding star lowering is follow-up work)"
        ));
    }
}

fn safe_div(a: f64, b: f64) -> f64 {
    if a > 0.0 && b > 0.0 { a / b } else { 1.0 }
}

/// The in-effect ratio factor of a ratio tap changer: SSH step, else the SV
/// tap step, else neutral.
fn ratio_tap_factor(mapper: &Mapper<'_>, rtc: &str) -> f64 {
    let store = mapper.store;
    let neutral = store.f(rtc, "TapChanger.neutralStep").unwrap_or(0.0);
    let step = store
        .f(rtc, "TapChanger.step")
        .or_else(|| sv_tap_step(store, rtc))
        .unwrap_or(neutral);
    let increment = store
        .f(rtc, "RatioTapChanger.stepVoltageIncrement")
        .unwrap_or(0.0);
    1.0 + (step - neutral) * increment / 100.0
}

fn sv_tap_step(store: &Store, tap_changer: &str) -> Option<f64> {
    store.of_class("SvTapStep").find_map(|sv| {
        (store.refv(sv, "SvTapStep.TapChanger") == Some(tap_changer))
            .then(|| store.f(sv, "SvTapStep.position"))
            .flatten()
    })
}

fn phase_tap_changer(store: &Store, end: &str) -> Option<String> {
    store
        .refv(end, "TransformerEnd.PhaseTapChanger")
        .map(str::to_string)
}

/// Best-effort phase shift in degrees. The linear changer is exact; the
/// asymmetrical/symmetrical/tabular families warn and use what they can.
fn phase_shift_deg(mapper: &mut Mapper<'_>, ptc: &str, invert: bool) -> f64 {
    let store = mapper.store;
    let class = store.class_of(ptc).unwrap_or("PhaseTapChanger").to_string();
    let neutral = store.f(ptc, "TapChanger.neutralStep").unwrap_or(0.0);
    let step = store
        .f(ptc, "TapChanger.step")
        .or_else(|| sv_tap_step(store, ptc))
        .unwrap_or(neutral);
    let degrees = match class.as_str() {
        "PhaseTapChangerLinear" => {
            (step - neutral)
                * store
                    .f(ptc, "PhaseTapChangerLinear.stepPhaseShiftIncrement")
                    .unwrap_or(0.0)
        }
        other => {
            mapper.warnings.push(format!(
                "{other} {}: non-linear phase tap changers are approximated as \
                 zero shift",
                store.name(ptc)
            ));
            0.0
        }
    };
    if invert { -degrees } else { degrees }
}

/// PATL → `rate_a`, TATL → `rate_b`, TC → `rate_c`, from the operational
/// limit sets on the equipment's terminals (or the equipment itself).
/// Current limits convert through √3·kV; apparent/active limits are MVA/MW.
fn apply_limits(
    mapper: &mut Mapper<'_>,
    equipment: &str,
    branch: &mut Branch,
    kv: f64,
    version: CgmesVersion,
) {
    let store = mapper.store;
    let mut terminals: Vec<&str> = mapper
        .wiring
        .terminals(equipment)
        .iter()
        .map(String::as_str)
        .collect();
    terminals.push(equipment);
    // Limit sets point at their terminal/equipment, so scan sets once.
    for set in store.of_class("OperationalLimitSet") {
        let target = store
            .refv(set, "OperationalLimitSet.Terminal")
            .or_else(|| store.refv(set, "OperationalLimitSet.Equipment"));
        if !target.is_some_and(|t| terminals.contains(&t)) {
            continue;
        }
        for limit in store.objects.iter().filter_map(|o| {
            matches!(
                o.class.as_str(),
                "CurrentLimit" | "ApparentPowerLimit" | "ActivePowerLimit"
            )
            .then_some(o.id.as_str())
        }) {
            if store.refv(limit, "OperationalLimit.OperationalLimitSet") != Some(set) {
                continue;
            }
            let Some(limit_type) = store.refv(limit, "OperationalLimit.OperationalLimitType")
            else {
                continue;
            };
            let kind = match version {
                CgmesVersion::V2_4_15 => {
                    store.enum_value(limit_type, "entsoe:OperationalLimitType.limitType")
                }
                CgmesVersion::V3_0 => store.enum_value(limit_type, "eu:OperationalLimitType.kind"),
            };
            let class = store.class_of(limit).unwrap_or_default();
            let value = store
                .f(limit, &format!("{class}.value"))
                .or_else(|| store.f(limit, &format!("{class}.normalValue")))
                .unwrap_or(0.0);
            let mva = if class == "CurrentLimit" {
                3f64.sqrt() * kv * value / 1000.0
            } else {
                value
            };
            let slot = match kind {
                Some("patl") => Some(&mut branch.rate_a),
                Some("tatl") => Some(&mut branch.rate_b),
                Some("tc" | "tct") => Some(&mut branch.rate_c),
                _ => None,
            };
            if let Some(slot) = slot {
                // Several sets can constrain one branch; the binding limit
                // is the smallest.
                if *slot == 0.0 || mva < *slot {
                    *slot = mva;
                }
            }
        }
    }
}

/// One warning per class the mapping did not consume, with its count.
fn warn_unmapped(store: &Store, warnings: &mut Vec<String>) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for object in &store.objects {
        let class = object.class.as_str();
        let consumed = CONSUMED.contains(&class)
            || SWITCH_CLASSES.contains(&class)
            || LOAD_CLASSES.contains(&class)
            || class.contains("Limit")
            || class.contains("TapChanger")
            || class.ends_with("GeneratingUnit")
            || matches!(
                class,
                // Containment/administrative hierarchy: implied by the
                // bus-branch view rather than dropped.
                "Substation"
                    | "VoltageLevel"
                    | "Bay"
                    | "BusbarSection"
                    | "Junction"
                    | "Line"
                    | "GeographicalRegion"
                    | "SubGeographicalRegion"
                    | "RegulatingControl"
                    | "TapChangerControl"
                    | "LoadResponseCharacteristic"
                    | "CoordinateSystem"
                    | "Location"
                    | "PositionPoint"
                    | "CurveData"
                    | "ReactiveCapabilityCurve"
                    | "LoadArea"
                    | "SubLoadArea"
                    | "LoadGroup"
                    | "ConformLoadGroup"
                    | "NonConformLoadGroup"
                    | "ControlArea"
                    | "TieFlow"
                    | "OperationalLimitSet"
                    | "OperationalLimitType"
                    | "FossilFuel"
                    | "EnergySchedulingType"
                    | "SeriesCompensator"
                    | "NonlinearShuntCompensator"
                    | "NonlinearShuntCompensatorPoint"
            );
        if !consumed {
            *counts.entry(class).or_default() += 1;
        }
    }
    for (class, count) in counts {
        warnings.push(format!(
            "{count} {class} object(s) have no balanced-model mapping and are \
             not carried"
        ));
    }
}
