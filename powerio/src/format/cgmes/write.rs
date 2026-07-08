//! [`Network`] to a CGMES instance file set: EQ, TP, SSH, and SV documents,
//! at either release, tied together by `md:FullModel` dependency headers.
//!
//! The writer is the reader's inverse. Bus-branch synthesis: one
//! `TopologicalNode` per bus (in TP, contained by a per-bus `VoltageLevel`
//! inside a minimal region/substation hierarchy in EQ), terminals linked to
//! nodes in TP, the operating point in SSH, and the solved state (plus the
//! island with its angle reference) in SV. Per-unit values leave through the
//! bus voltage bases onto ohms/siemens; CGMES carries no system MVA base, so
//! a write from a base other than 100 MVA warns that a reparse re-bases.
//!
//! mRIDs are deterministic: an element's imported CIM id (its `uid`) passes
//! through, and everything else derives a UUID-shaped id from its role and
//! name — so canonical output is byte-idempotent and diffs are meaningful.
//! Header timestamps are a fixed sentinel for the same reason.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use super::CgmesVersion;
use crate::network::{BusId, BusType, Network};
use crate::{Error, Result};

/// The emitted profile documents, `(file_name, xml)` in EQ/TP/SSH/SV order,
/// plus every fidelity loss the writer took.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CgmesFiles {
    pub files: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

/// Deterministic output needs a fixed header timestamp; consumers read it as
/// provenance metadata, not data.
const STAMP: &str = "2000-01-01T00:00:00Z";

fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = seed;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// A UUID-shaped identifier derived from an object's role and name, so writes
/// are reproducible; imported mRIDs (element `uid`s) take precedence at the
/// call sites.
fn det_mrid(kind: &str, name: &str) -> String {
    let tag = format!("powerio-cgmes:{kind}:{name}");
    let hi = fnv1a(0xcbf2_9ce4_8422_2325, tag.as_bytes());
    let lo = fnv1a(0x6c62_272e_07bb_0142, tag.as_bytes());
    let hi = (hi & 0xFFFF_FFFF_FFFF_0FFF) | 0x0000_0000_0000_4000;
    let lo = (lo & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        hi >> 32,
        (hi >> 16) & 0xFFFF,
        hi & 0xFFFF,
        lo >> 48,
        lo & 0xFFFF_FFFF_FFFF
    )
}

/// The imported mRID when the element carries one, else deterministic.
fn mrid_or(kind: &str, name: &str, uid: Option<&str>) -> String {
    uid.map_or_else(|| det_mrid(kind, name), str::to_owned)
}

fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// One profile document under construction.
struct Doc {
    body: String,
}

impl Doc {
    fn new() -> Doc {
        Doc {
            body: String::new(),
        }
    }

    /// Open an object element (`rdf:ID` definition or `rdf:about` extension).
    fn open(&mut self, class: &str, id: &str, about: bool) {
        let attr = if about {
            format!("rdf:about=\"#_{id}\"")
        } else {
            format!("rdf:ID=\"_{id}\"")
        };
        let _ = writeln!(self.body, "  <cim:{class} {attr}>");
    }

    fn close(&mut self, class: &str) {
        let _ = writeln!(self.body, "  </cim:{class}>");
    }

    fn text(&mut self, prop: &str, value: impl std::fmt::Display) {
        let _ = writeln!(
            self.body,
            "    <cim:{prop}>{}</cim:{prop}>",
            esc(&value.to_string())
        );
    }

    /// A property in a non-`cim` namespace (`entsoe:`/`eu:` extensions).
    fn ext_ref(&mut self, prefix: &str, prop: &str, uri: &str) {
        let _ = writeln!(self.body, "    <{prefix}:{prop} rdf:resource=\"{uri}\"/>");
    }

    fn reference(&mut self, prop: &str, target: &str) {
        let _ = writeln!(self.body, "    <cim:{prop} rdf:resource=\"#_{target}\"/>");
    }

    fn enumeration(&mut self, prop: &str, cim_ns: &str, value: &str) {
        let _ = writeln!(
            self.body,
            "    <cim:{prop} rdf:resource=\"{cim_ns}{value}\"/>"
        );
    }

    fn named(&mut self, class: &str, id: &str, name: &str) {
        self.open(class, id, false);
        self.text("IdentifiedObject.name", esc(name));
    }
}

struct Profiles {
    cim_ns: &'static str,
    ext: (&'static str, &'static str), // (prefix, namespace)
    eq: &'static str,
    tp: &'static str,
    ssh: &'static str,
    sv: &'static str,
}

fn profiles(version: CgmesVersion) -> Profiles {
    match version {
        CgmesVersion::V2_4_15 => Profiles {
            cim_ns: "http://iec.ch/TC57/2013/CIM-schema-cim16#",
            ext: ("entsoe", "http://entsoe.eu/CIM/SchemaExtension/3/1#"),
            eq: "http://entsoe.eu/CIM/EquipmentCore/3/1",
            tp: "http://entsoe.eu/CIM/Topology/4/1",
            ssh: "http://entsoe.eu/CIM/SteadyStateHypothesis/1/1",
            sv: "http://entsoe.eu/CIM/StateVariables/4/1",
        },
        CgmesVersion::V3_0 => Profiles {
            cim_ns: "http://iec.ch/TC57/CIM100#",
            ext: ("eu", "http://iec.ch/TC57/CIM100-European#"),
            eq: "http://iec.ch/TC57/ns/CIM/CoreEquipment-EU/3.0",
            tp: "http://iec.ch/TC57/ns/CIM/Topology-EU/3.0",
            ssh: "http://iec.ch/TC57/ns/CIM/SteadyStateHypothesis-EU/3.0",
            sv: "http://iec.ch/TC57/ns/CIM/StateVariables-EU/3.0",
        },
    }
}

/// Wrap a profile body in its `rdf:RDF` envelope with the `md:FullModel`
/// header (profile URI, sentinel timestamps, dependency links).
fn document(
    p: &Profiles,
    profile: &str,
    model_id: &str,
    description: &str,
    depends: &[&str],
    body: &str,
) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        out,
        "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"\n         \
         xmlns:cim=\"{}\"\n         xmlns:{}=\"{}\"\n         \
         xmlns:md=\"http://iec.ch/TC57/61970-552/ModelDescription/1#\">",
        p.cim_ns, p.ext.0, p.ext.1
    );
    let _ = writeln!(out, "  <md:FullModel rdf:about=\"urn:uuid:{model_id}\">");
    let _ = writeln!(
        out,
        "    <md:Model.scenarioTime>{STAMP}</md:Model.scenarioTime>"
    );
    let _ = writeln!(out, "    <md:Model.created>{STAMP}</md:Model.created>");
    let _ = writeln!(
        out,
        "    <md:Model.description>{}</md:Model.description>",
        esc(description)
    );
    let _ = writeln!(out, "    <md:Model.version>1</md:Model.version>");
    let _ = writeln!(out, "    <md:Model.profile>{profile}</md:Model.profile>");
    for dep in depends {
        let _ = writeln!(
            out,
            "    <md:Model.DependentOn rdf:resource=\"urn:uuid:{dep}\"/>"
        );
    }
    let _ = writeln!(
        out,
        "    <md:Model.modelingAuthoritySet>http://powerio.dev/cgmes</md:Model.modelingAuthoritySet>"
    );
    out.push_str("  </md:FullModel>\n");
    out.push_str(body);
    out.push_str("</rdf:RDF>\n");
    out
}

/// The per-element naming and unit conversions shared by the four profiles.
struct Writer<'a> {
    net: &'a Network,
    p: Profiles,
    warnings: Vec<String>,
}

impl Writer<'_> {
    fn kv(&self, bus: BusId) -> f64 {
        let kv = self
            .net
            .buses
            .iter()
            .find(|b| b.id == bus)
            .map_or(0.0, |b| b.base_kv);
        if kv > 0.0 { kv } else { 1.0 }
    }

    fn raw_kv(&self, bus: BusId) -> f64 {
        self.net
            .buses
            .iter()
            .find(|b| b.id == bus)
            .map_or(0.0, |b| b.base_kv)
    }
}

/// A bus's TopologicalNode id: the imported uid, else deterministic.
fn bus_mrid(net: &Network, bus: BusId) -> String {
    net.buses
        .iter()
        .find(|b| b.id == bus)
        .map(|b| {
            b.uid
                .clone()
                .unwrap_or_else(|| det_mrid("bus", &b.id.to_string()))
        })
        .unwrap_or_default()
}

/// Terminal id for equipment `eq` at sequence `seq`.
fn term_id(eq: &str, seq: usize) -> String {
    det_mrid("terminal", &format!("{eq}:{seq}"))
}

/// Serialize `net` as a CGMES EQ/TP/SSH/SV file set at `version`.
///
/// Every model field the profile set cannot carry is reported in the
/// warnings; nothing drops silently.
#[must_use]
#[allow(clippy::too_many_lines)] // the four profiles emit in one ordered pass
#[allow(clippy::if_not_else)] // the line arm reads first, matching the reader
pub fn write_cgmes(net: &Network, version: CgmesVersion) -> CgmesFiles {
    let p = profiles(version);
    let v3 = version == CgmesVersion::V3_0;
    let mut w = Writer {
        net,
        p,
        warnings: Vec::new(),
    };
    let mut eq = Doc::new();
    let mut tp = Doc::new();
    let mut ssh = Doc::new();
    let mut sv = Doc::new();

    if (net.base_mva - 100.0).abs() > 1e-9 {
        w.warnings.push(format!(
            "system base {} MVA: CGMES carries no MVA base, so a reparse lands \
             per-unit values on 100 MVA",
            net.base_mva
        ));
    }
    if net.buses.iter().any(|b| b.base_kv <= 0.0) {
        w.warnings.push(
            "some buses carry base_kv 0; the SI conversions use 1 kV for them, so \
             ohm/siemens/volt magnitudes are not physical (per-unit values still \
             round-trip)"
                .into(),
        );
    }

    // --- containment + bases (EQ) ---------------------------------------
    let region = det_mrid("region", "GR");
    eq.named("GeographicalRegion", &region, "GR");
    eq.close("GeographicalRegion");
    let subregion = det_mrid("region", "SGR");
    eq.named("SubGeographicalRegion", &subregion, "SGR");
    eq.reference("SubGeographicalRegion.Region", &region);
    eq.close("SubGeographicalRegion");
    let mut base_ids: Vec<(f64, String)> = Vec::new();
    for bus in &net.buses {
        if !base_ids
            .iter()
            .any(|(kv, _)| kv.to_bits() == bus.base_kv.to_bits())
        {
            let id = det_mrid("basevoltage", &format!("{}", bus.base_kv));
            eq.named("BaseVoltage", &id, &format!("{} kV", bus.base_kv));
            eq.text("BaseVoltage.nominalVoltage", bus.base_kv);
            eq.close("BaseVoltage");
            base_ids.push((bus.base_kv, id));
        }
    }
    let base_of = |kv: f64| -> String {
        base_ids
            .iter()
            .find(|(k, _)| k.to_bits() == kv.to_bits())
            .map(|(_, id)| id.clone())
            .unwrap_or_default()
    };
    let freq = det_mrid("frequency", "base");
    eq.named(
        "BaseFrequency",
        &freq,
        &format!("{} Hz", net.base_frequency),
    );
    eq.text("BaseFrequency.frequency", net.base_frequency);
    eq.close("BaseFrequency");

    // Per bus: Substation + VoltageLevel (the TN's container), TN in TP,
    // SvVoltage in SV.
    for bus in &net.buses {
        let sub = det_mrid("substation", &bus.id.to_string());
        eq.named("Substation", &sub, &format!("S{}", bus.id));
        eq.reference("Substation.Region", &subregion);
        eq.close("Substation");
        let vl = det_mrid("voltagelevel", &bus.id.to_string());
        eq.named("VoltageLevel", &vl, &format!("VL{}", bus.id));
        eq.reference("VoltageLevel.Substation", &sub);
        eq.reference("VoltageLevel.BaseVoltage", &base_of(bus.base_kv));
        eq.close("VoltageLevel");

        let tn = bus_mrid(net, bus.id);
        tp.named(
            "TopologicalNode",
            &tn,
            bus.name.as_deref().unwrap_or(&bus.id.to_string()),
        );
        tp.reference("TopologicalNode.BaseVoltage", &base_of(bus.base_kv));
        tp.reference("TopologicalNode.ConnectivityNodeContainer", &vl);
        tp.close("TopologicalNode");

        let kv = if bus.base_kv > 0.0 { bus.base_kv } else { 1.0 };
        let svv = det_mrid("svvoltage", &bus.id.to_string());
        sv.open("SvVoltage", &svv, false);
        sv.reference("SvVoltage.TopologicalNode", &tn);
        sv.text("SvVoltage.v", bus.vm * kv);
        sv.text("SvVoltage.angle", bus.va);
        sv.close("SvVoltage");

        if bus.evhi.is_some() || bus.evlo.is_some() {
            w.warnings.push(format!(
                "bus {}: emergency voltage band (evhi/evlo) has no CGMES slot",
                bus.id
            ));
        }
    }
    let tn_of = |bus: BusId| -> String { bus_mrid(net, bus) };

    // A terminal: EQ definition + TP node link + SSH connected state.
    let terminal = |eq: &mut Doc,
                    tp: &mut Doc,
                    ssh: &mut Doc,
                    owner: &str,
                    seq: usize,
                    bus: BusId,
                    connected: bool| {
        let id = term_id(owner, seq);
        eq.open("Terminal", &id, false);
        eq.reference("Terminal.ConductingEquipment", owner);
        eq.text("ACDCTerminal.sequenceNumber", seq);
        eq.close("Terminal");
        tp.open("Terminal", &id, true);
        tp.reference("Terminal.TopologicalNode", &tn_of(bus));
        tp.close("Terminal");
        ssh.open("Terminal", &id, true);
        ssh.text("ACDCTerminal.connected", connected);
        ssh.close("Terminal");
        id
    };

    // --- operational limit plumbing --------------------------------------
    let ext_ns = w.p.ext.1;
    let limit_kind_uri = move |kind: &str| -> String {
        if v3 {
            format!("{ext_ns}LimitKind.{kind}")
        } else {
            format!("{ext_ns}LimitTypeKind.{kind}")
        }
    };
    let mut limit_types_used: Vec<&'static str> = Vec::new();
    let mut limit_doc = Doc::new();

    // --- loads ------------------------------------------------------------
    for (i, load) in net.loads.iter().enumerate() {
        let id = mrid_or("load", &format!("{}-{i}", load.bus), load.uid.as_deref());
        eq.named("EnergyConsumer", &id, &format!("load{}-{i}", load.bus));
        eq.reference(
            "Equipment.EquipmentContainer",
            &det_mrid("voltagelevel", &load.bus.to_string()),
        );
        eq.close("EnergyConsumer");
        terminal(
            &mut eq,
            &mut tp,
            &mut ssh,
            &id,
            1,
            load.bus,
            load.in_service,
        );
        ssh.open("EnergyConsumer", &id, true);
        ssh.text("EnergyConsumer.p", load.p);
        ssh.text("EnergyConsumer.q", load.q);
        if v3 {
            ssh.text("Equipment.inService", load.in_service);
        }
        ssh.close("EnergyConsumer");
        if load.voltage_model.is_some() {
            w.warnings.push(format!(
                "load at bus {}: voltage-dependence model is not written (no \
                 LoadResponseCharacteristic mapping yet)",
                load.bus
            ));
        }
    }

    // --- generators --------------------------------------------------------
    for (i, machine) in net.generators.iter().enumerate() {
        let id = mrid_or(
            "gen",
            &format!("{}-{i}", machine.bus),
            machine.uid.as_deref(),
        );
        let unit = det_mrid("genunit", &id);
        eq.named(
            "GeneratingUnit",
            &unit,
            &format!("gen{}-{i}-unit", machine.bus),
        );
        eq.text("GeneratingUnit.minOperatingP", machine.pmin);
        eq.text("GeneratingUnit.maxOperatingP", machine.pmax);
        eq.close("GeneratingUnit");
        let control = det_mrid("regcontrol", &id);
        eq.named(
            "SynchronousMachine",
            &id,
            &format!("gen{}-{i}", machine.bus),
        );
        eq.text("SynchronousMachine.maxQ", machine.qmax);
        eq.text("SynchronousMachine.minQ", machine.qmin);
        if machine.mbase > 0.0 {
            eq.text("RotatingMachine.ratedS", machine.mbase);
        }
        eq.reference("RotatingMachine.GeneratingUnit", &unit);
        eq.reference("RegulatingCondEq.RegulatingControl", &control);
        eq.close("SynchronousMachine");
        let term = terminal(
            &mut eq,
            &mut tp,
            &mut ssh,
            &id,
            1,
            machine.bus,
            machine.in_service,
        );
        eq.named(
            "RegulatingControl",
            &control,
            &format!("gen{}-{i}-avr", machine.bus),
        );
        eq.enumeration(
            "RegulatingControl.mode",
            w.p.cim_ns,
            "RegulatingControlModeKind.voltage",
        );
        eq.reference("RegulatingControl.Terminal", &term);
        eq.close("RegulatingControl");
        ssh.open("SynchronousMachine", &id, true);
        ssh.text("RotatingMachine.p", -machine.pg);
        ssh.text("RotatingMachine.q", -machine.qg);
        if net
            .buses
            .iter()
            .any(|b| b.id == machine.bus && b.kind == BusType::Ref)
        {
            ssh.text("SynchronousMachine.referencePriority", 1);
        }
        if v3 {
            ssh.text("Equipment.inService", machine.in_service);
        }
        ssh.close("SynchronousMachine");
        ssh.open("RegulatingControl", &control, true);
        ssh.text("RegulatingControl.enabled", true);
        ssh.text(
            "RegulatingControl.targetValue",
            machine.vg * w.kv(machine.bus),
        );
        ssh.close("RegulatingControl");
        if machine.regulated_bus.is_some_and(|b| b != machine.bus) {
            w.warnings.push(format!(
                "generator at bus {}: remote regulated bus is written as local \
                 regulation (the control terminal is the machine's own)",
                machine.bus
            ));
        }
        if machine.cost.is_some() {
            w.warnings.push(format!(
                "generator at bus {}: cost curves have no CGMES slot",
                machine.bus
            ));
        }
        if machine.has_caps() {
            w.warnings.push(format!(
                "generator at bus {}: capability/ramp columns have no CGMES slot",
                machine.bus
            ));
        }
    }

    // --- shunts -------------------------------------------------------------
    for (i, shunt) in net.shunts.iter().enumerate() {
        let id = mrid_or("shunt", &format!("{}-{i}", shunt.bus), shunt.uid.as_deref());
        let kv = w.kv(shunt.bus);
        eq.named(
            "LinearShuntCompensator",
            &id,
            &format!("shunt{}-{i}", shunt.bus),
        );
        eq.text("LinearShuntCompensator.bPerSection", shunt.b / (kv * kv));
        eq.text("LinearShuntCompensator.gPerSection", shunt.g / (kv * kv));
        eq.text("ShuntCompensator.maximumSections", 1);
        eq.text("ShuntCompensator.normalSections", 1);
        eq.text("ShuntCompensator.nomU", w.raw_kv(shunt.bus));
        eq.close("LinearShuntCompensator");
        terminal(
            &mut eq,
            &mut tp,
            &mut ssh,
            &id,
            1,
            shunt.bus,
            shunt.in_service,
        );
        ssh.open("LinearShuntCompensator", &id, true);
        ssh.text("ShuntCompensator.sections", 1);
        if v3 {
            ssh.text("Equipment.inService", shunt.in_service);
        }
        ssh.close("LinearShuntCompensator");
        if shunt.control.is_some() {
            w.warnings.push(format!(
                "shunt at bus {}: switched-shunt control blocks are written as a \
                 fixed single-section compensator",
                shunt.bus
            ));
        }
    }

    // --- switches -------------------------------------------------------------
    for (i, switch) in net.switches.iter().enumerate() {
        let id = mrid_or(
            "switch",
            &format!("{}-{}-{i}", switch.from, switch.to),
            switch.uid.as_deref(),
        );
        eq.named(
            "Breaker",
            &id,
            &format!("switch{}-{}-{i}", switch.from, switch.to),
        );
        eq.text("Switch.normalOpen", !switch.closed);
        eq.text("Switch.retained", true);
        if let Some(amps) = switch.current_rating {
            eq.text("Switch.ratedCurrent", amps);
        }
        eq.close("Breaker");
        terminal(&mut eq, &mut tp, &mut ssh, &id, 1, switch.from, true);
        terminal(&mut eq, &mut tp, &mut ssh, &id, 2, switch.to, true);
        ssh.open("Breaker", &id, true);
        ssh.text("Switch.open", !switch.closed);
        ssh.close("Breaker");
    }

    // --- branches ---------------------------------------------------------------
    let mut limit_body = Doc::new();
    for (i, branch) in net.branches.iter().enumerate() {
        let id = mrid_or(
            "branch",
            &format!("{}-{}-{i}", branch.from, branch.to),
            branch.uid.as_deref(),
        );
        let kv = w.kv(branch.from);
        let z_base = kv * kv / net.base_mva;
        let y_base = net.base_mva / (kv * kv);
        let charging = branch.terminal_charging();
        if !charging.is_matpower_symmetric() {
            w.warnings.push(format!(
                "branch {} ({}-{}): asymmetric terminal charging folded into the \
                 symmetric bch/gch totals",
                i + 1,
                branch.from,
                branch.to
            ));
        }
        if branch.control.is_some() {
            w.warnings.push(format!(
                "branch {} ({}-{}): automatic tap/phase control data is not \
                 written (fixed in-service step only)",
                i + 1,
                branch.from,
                branch.to
            ));
        }
        if !branch.is_transformer() {
            eq.named("ACLineSegment", &id, &format!("line{}", i + 1));
            eq.text("ACLineSegment.r", branch.r * z_base);
            eq.text("ACLineSegment.x", branch.x * z_base);
            eq.text(
                "ACLineSegment.bch",
                branch.legacy_total_charging_b() * y_base,
            );
            let g_total = charging.total_g();
            if g_total != 0.0 {
                eq.text("ACLineSegment.gch", g_total * y_base);
            }
            eq.reference(
                "ConductingEquipment.BaseVoltage",
                &base_of(w.raw_kv(branch.from)),
            );
            eq.close("ACLineSegment");
        } else {
            // Two-winding transformer: the MATPOWER tap folds into the end-1
            // rated voltage (reader ratio = (u1/kv1)/(u2/kv2)); the phase
            // shift rides a one-step linear phase tap changer on end 1.
            let (u1, u2) = (w.kv(branch.from) * branch.effective_tap(), w.kv(branch.to));
            eq.named("PowerTransformer", &id, &format!("transformer{}", i + 1));
            eq.close("PowerTransformer");
            for (endno, u) in [(1usize, u1), (2usize, u2)] {
                let end = det_mrid("xfend", &format!("{id}:{endno}"));
                eq.named(
                    "PowerTransformerEnd",
                    &end,
                    &format!("transformer{}-end{endno}", i + 1),
                );
                eq.reference("PowerTransformerEnd.PowerTransformer", &id);
                eq.text("TransformerEnd.endNumber", endno);
                eq.reference("TransformerEnd.Terminal", &term_id(&id, endno));
                eq.text("PowerTransformerEnd.ratedU", u);
                if endno == 1 {
                    let zb1 = u * u / net.base_mva;
                    eq.text("PowerTransformerEnd.r", branch.r * zb1);
                    eq.text("PowerTransformerEnd.x", branch.x * zb1);
                    let b_total = branch.legacy_total_charging_b();
                    if b_total != 0.0 {
                        eq.text("PowerTransformerEnd.b", b_total / zb1);
                    }
                    if branch.shift != 0.0 {
                        let ptc = det_mrid("ptc", &id);
                        eq.reference("TransformerEnd.PhaseTapChanger", &ptc);
                    }
                } else {
                    eq.text("PowerTransformerEnd.r", 0.0);
                    eq.text("PowerTransformerEnd.x", 0.0);
                }
                eq.close("PowerTransformerEnd");
            }
            if branch.shift != 0.0 {
                let ptc = det_mrid("ptc", &id);
                eq.named(
                    "PhaseTapChangerLinear",
                    &ptc,
                    &format!("transformer{}-shift", i + 1),
                );
                eq.text("TapChanger.lowStep", 0);
                eq.text("TapChanger.highStep", 2);
                eq.text("TapChanger.neutralStep", 0);
                eq.text("TapChanger.normalStep", 1);
                eq.text("TapChanger.neutralU", u1);
                eq.text("TapChanger.ltcFlag", false);
                eq.text(
                    "PhaseTapChangerLinear.stepPhaseShiftIncrement",
                    branch.shift,
                );
                eq.text("PhaseTapChangerLinear.xMin", branch.x * z_base);
                eq.text("PhaseTapChangerLinear.xMax", branch.x * z_base);
                eq.close("PhaseTapChangerLinear");
                ssh.open("PhaseTapChangerLinear", &ptc, true);
                ssh.text("TapChanger.step", 1);
                ssh.text("TapChanger.controlEnabled", false);
                ssh.close("PhaseTapChangerLinear");
                sv.open("SvTapStep", &det_mrid("svtap", &ptc), false);
                sv.reference("SvTapStep.TapChanger", &ptc);
                sv.text("SvTapStep.position", 1);
                sv.close("SvTapStep");
            }
        }
        terminal(
            &mut eq,
            &mut tp,
            &mut ssh,
            &id,
            1,
            branch.from,
            branch.in_service,
        );
        terminal(
            &mut eq,
            &mut tp,
            &mut ssh,
            &id,
            2,
            branch.to,
            branch.in_service,
        );
        if v3 {
            let class = if branch.is_transformer() {
                "PowerTransformer"
            } else {
                "ACLineSegment"
            };
            ssh.open(class, &id, true);
            ssh.text("Equipment.inService", branch.in_service);
            ssh.close(class);
        }

        // PATL/TATL/TC current limits at terminal 1 through √3·kV.
        let mut rate = |mva: f64, kind: &'static str, w: &mut Writer<'_>| {
            if mva <= 0.0 {
                return;
            }
            if !limit_types_used.contains(&kind) {
                limit_types_used.push(kind);
            }
            let set = det_mrid("limitset", &format!("{id}:{kind}"));
            limit_body.named(
                "OperationalLimitSet",
                &set,
                &format!("limits-{}-{kind}", i + 1),
            );
            limit_body.reference("OperationalLimitSet.Terminal", &term_id(&id, 1));
            limit_body.close("OperationalLimitSet");
            let lim = det_mrid("limit", &format!("{id}:{kind}"));
            limit_body.named("CurrentLimit", &lim, &format!("rate-{}-{kind}", i + 1));
            limit_body.reference("OperationalLimit.OperationalLimitSet", &set);
            limit_body.reference(
                "OperationalLimit.OperationalLimitType",
                &det_mrid("limittype", kind),
            );
            let amps = mva * 1000.0 / (3f64.sqrt() * w.kv(branch.from));
            if v3 {
                limit_body.text("CurrentLimit.normalValue", amps);
            } else {
                limit_body.text("CurrentLimit.value", amps);
            }
            limit_body.close("CurrentLimit");
        };
        rate(branch.rate_a, "patl", &mut w);
        rate(branch.rate_b, "tatl", &mut w);
        rate(branch.rate_c, "tc", &mut w);
        if !branch.rating_sets.is_empty() || branch.current_ratings.is_some() {
            w.warnings.push(format!(
                "branch {} ({}-{}): extra rating sets / current ratings beyond \
                 A/B/C have no CGMES slot",
                i + 1,
                branch.from,
                branch.to
            ));
        }
    }
    for kind in &limit_types_used {
        let id = det_mrid("limittype", kind);
        limit_doc.named("OperationalLimitType", &id, kind);
        limit_doc.enumeration(
            "OperationalLimitType.direction",
            w.p.cim_ns,
            "OperationalLimitDirectionKind.absoluteValue",
        );
        if v3 {
            limit_doc.ext_ref(
                w.p.ext.0,
                "OperationalLimitType.kind",
                &limit_kind_uri(kind),
            );
            limit_doc.text("OperationalLimitType.isInfiniteDuration", *kind == "patl");
        } else {
            limit_doc.ext_ref(
                w.p.ext.0,
                "OperationalLimitType.limitType",
                &limit_kind_uri(kind),
            );
            limit_doc.text("OperationalLimitType.acceptableDuration", 900);
        }
        limit_doc.close("OperationalLimitType");
    }
    eq.body.push_str(&limit_doc.body);
    eq.body.push_str(&limit_body.body);

    // --- island (SV) --------------------------------------------------------
    let refs: Vec<&crate::network::Bus> = net
        .buses
        .iter()
        .filter(|b| b.kind == BusType::Ref)
        .collect();
    if let Some(slack) = refs.first() {
        let island = det_mrid("island", "1");
        sv.named("TopologicalIsland", &island, "island");
        sv.reference(
            "TopologicalIsland.AngleRefTopologicalNode",
            &bus_mrid(net, slack.id),
        );
        for bus in &net.buses {
            sv.reference("TopologicalIsland.TopologicalNodes", &bus_mrid(net, bus.id));
        }
        sv.close("TopologicalIsland");
    } else {
        w.warnings
            .push("no reference bus: the SV island has no angle reference".into());
    }

    // --- unrepresented families ------------------------------------------------
    for (what, count) in [
        ("storage unit", net.storage.len()),
        ("HVDC line", net.hvdc.len()),
        ("three-winding transformer", net.transformers_3w.len()),
        (
            "area record",
            net.areas
                .iter()
                .filter(|a| a.slack_bus.is_some() || a.net_interchange != 0.0)
                .count(),
        ),
        ("solver-parameter block", usize::from(net.solver.is_some())),
    ] {
        if count > 0 {
            w.warnings.push(format!(
                "{count} {what}(s) have no CGMES mapping yet and are dropped"
            ));
        }
    }

    let name = if net.name.is_empty() {
        "case"
    } else {
        &net.name
    };
    let stem: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let ids: Vec<String> = ["EQ", "TP", "SSH", "SV"]
        .iter()
        .map(|part| det_mrid("model", &format!("{stem}:{part}")))
        .collect();
    let files = vec![
        (
            format!("{stem}_EQ.xml"),
            document(&w.p, w.p.eq, &ids[0], name, &[], &eq.body),
        ),
        (
            format!("{stem}_TP.xml"),
            document(&w.p, w.p.tp, &ids[1], name, &[&ids[0]], &tp.body),
        ),
        (
            format!("{stem}_SSH.xml"),
            document(&w.p, w.p.ssh, &ids[2], name, &[&ids[0]], &ssh.body),
        ),
        (
            format!("{stem}_SV.xml"),
            document(&w.p, w.p.sv, &ids[3], name, &[&ids[1], &ids[2]], &sv.body),
        ),
    ];
    CgmesFiles {
        files,
        warnings: w.warnings,
    }
}

/// [`write_cgmes`] straight to `dir` (created if absent); returns the paths.
///
/// # Errors
/// [`Error::Io`] on filesystem failures.
pub fn write_cgmes_dir(
    net: &Network,
    version: CgmesVersion,
    dir: impl AsRef<Path>,
) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let dir = dir.as_ref();
    std::fs::create_dir_all(dir).map_err(Error::from)?;
    let out = write_cgmes(net, version);
    let mut paths = Vec::new();
    for (name, text) in &out.files {
        let path = dir.join(name);
        std::fs::write(&path, text).map_err(Error::from)?;
        paths.push(path);
    }
    Ok((paths, out.warnings))
}
