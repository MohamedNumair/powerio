//! [`DistNetwork`] to one distribution CIM (CIM100) CIMXML document: the
//! reader's inverse, GridAPPS-D-style per-phase classes.
//!
//! mRIDs are deterministic: an element's imported CIM id (its `cim_mrid`
//! extra) passes through, and everything else derives a UUID-shaped id from
//! its role and name — write → read → write is byte stable. Wire-coordinate
//! terminal names map back to `SinglePhaseKind` (`1`→A, `2`→B, `3`→C,
//! `4`→N); the grounded neutral is implied by the wye phase children, so it
//! is not emitted as a phase.

use std::fmt::Write as _;

use crate::convert::Conversion;
use crate::model::{Configuration, DistNetwork, Extras, Mat, WindingConn};

const CIM_NS: &str = "http://iec.ch/TC57/CIM100#";

fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = seed;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// UUID-shaped deterministic id from an object's role and name.
fn det_mrid(kind: &str, name: &str) -> String {
    let tag = format!("powerio-distcim:{kind}:{name}");
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

/// The imported mRID (the `cim_mrid` extra) when present, else deterministic.
fn mrid_of(extras: &Extras, kind: &str, name: &str) -> String {
    extras
        .get("cim_mrid")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| det_mrid(kind, name), str::to_owned)
}

fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn phase_letter(terminal: &str) -> Option<&'static str> {
    match terminal {
        "1" => Some("A"),
        "2" => Some("B"),
        "3" => Some("C"),
        "4" => Some("N"),
        _ => None,
    }
}

/// The non-neutral phase letters of a terminal map, in map order.
fn phases_of(map: &[String]) -> Vec<&'static str> {
    map.iter()
        .filter(|t| t.as_str() != "4")
        .filter_map(|t| phase_letter(t))
        .collect()
}

struct Doc {
    body: String,
    warnings: Vec<String>,
}

impl Doc {
    fn open(&mut self, class: &str, id: &str) {
        let _ = writeln!(self.body, "  <cim:{class} rdf:ID=\"_{id}\">");
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

    fn reference(&mut self, prop: &str, target: &str) {
        let _ = writeln!(self.body, "    <cim:{prop} rdf:resource=\"#_{target}\"/>");
    }

    fn enumeration(&mut self, prop: &str, value: &str) {
        let _ = writeln!(
            self.body,
            "    <cim:{prop} rdf:resource=\"{CIM_NS}{value}\"/>"
        );
    }

    fn named(&mut self, class: &str, id: &str, name: &str) {
        self.open(class, id);
        self.text("IdentifiedObject.mRID", id);
        self.text("IdentifiedObject.name", esc(name));
    }

    fn terminal(&mut self, owner: &str, seq: usize, node: &str, phases: Option<&str>) {
        let id = det_mrid("terminal", &format!("{owner}:{seq}"));
        self.open("Terminal", &id);
        self.reference("Terminal.ConductingEquipment", owner);
        self.reference("Terminal.ConnectivityNode", node);
        self.text("ACDCTerminal.sequenceNumber", seq);
        if let Some(code) = phases {
            self.enumeration("Terminal.phases", &format!("PhaseCode.{code}"));
        }
        self.close("Terminal");
    }

    fn warn_extras(&mut self, kind: &str, name: &str, extras: &Extras) {
        let keys: Vec<&str> = extras
            .keys()
            .map(String::as_str)
            .filter(|k| *k != "cim_mrid")
            .collect();
        if !keys.is_empty() {
            self.warnings.push(format!(
                "{kind} {name}: extras [{}] have no distribution CIM mapping",
                keys.join(", ")
            ));
        }
    }
}

/// The composite `PhaseCode` value for a terminal map (`ABC`, `ABCN`, `A`…).
fn phase_code(map: &[String]) -> String {
    let mut code = String::new();
    for terminal in map {
        if let Some(letter) = phase_letter(terminal) {
            code.push_str(letter);
        }
    }
    if code.is_empty() { "ABC".into() } else { code }
}

/// Serialize `net` as one distribution CIM document. Everything the profile
/// cannot carry is reported in the warnings; nothing drops silently.
#[must_use]
#[allow(clippy::too_many_lines)] // one element family per block, in model order
pub fn write_cim_xml(net: &DistNetwork) -> Conversion {
    let mut doc = Doc {
        body: String::new(),
        warnings: Vec::new(),
    };

    let node_id = |net: &DistNetwork, bus: &str| -> String {
        net.buses
            .iter()
            .find(|b| b.id.eq_ignore_ascii_case(bus))
            .map_or_else(
                || det_mrid("node", bus),
                |b| mrid_of(&b.extras, "node", &b.id),
            )
    };

    doc.open("BaseFrequency", &det_mrid("frequency", "base"));
    doc.text(
        "IdentifiedObject.name",
        format!("{} Hz", net.base_frequency),
    );
    doc.text("BaseFrequency.frequency", net.base_frequency);
    doc.close("BaseFrequency");

    for bus in &net.buses {
        let id = mrid_of(&bus.extras, "node", &bus.id);
        doc.named("ConnectivityNode", &id, &bus.id);
        doc.close("ConnectivityNode");
        doc.warn_extras("bus", &bus.id, &bus.extras);
    }

    // Line codes → PerLengthPhaseImpedance with lower-triangle data rows.
    for lc in &net.linecodes {
        let id = mrid_of(&lc.extras, "plpi", &lc.name);
        doc.named("PerLengthPhaseImpedance", &id, &lc.name);
        doc.text("PerLengthPhaseImpedance.conductorCount", lc.n_conductors);
        doc.close("PerLengthPhaseImpedance");
        let total_b = |i: usize, j: usize| -> f64 {
            let at = |m: &Mat| m.get(i).and_then(|r| r.get(j)).copied().unwrap_or(0.0);
            at(&lc.b_from) + at(&lc.b_to)
        };
        for i in 0..lc.n_conductors {
            for j in 0..=i {
                let did = det_mrid("pid", &format!("{}:{i}:{j}", lc.name));
                doc.open("PhaseImpedanceData", &did);
                doc.reference("PhaseImpedanceData.PerLengthPhaseImpedance", &id);
                doc.text("PhaseImpedanceData.row", i + 1);
                doc.text("PhaseImpedanceData.column", j + 1);
                doc.text("PhaseImpedanceData.r", lc.r_series[i][j]);
                doc.text("PhaseImpedanceData.x", lc.x_series[i][j]);
                doc.text("PhaseImpedanceData.b", total_b(i, j));
                doc.close("PhaseImpedanceData");
            }
        }
        let asymmetric = lc
            .g_from
            .iter()
            .flatten()
            .chain(lc.g_to.iter().flatten())
            .any(|v| *v != 0.0);
        if asymmetric {
            doc.warnings.push(format!(
                "linecode {}: shunt conductance has no PhaseImpedanceData slot; \
                 only b is written",
                lc.name
            ));
        }
        if lc.i_max.is_some() || lc.s_max.is_some() {
            doc.warnings.push(format!(
                "linecode {}: ampacity/apparent-power ratings are not written \
                 (no OperationalLimit mapping yet)",
                lc.name
            ));
        }
        doc.warn_extras("linecode", &lc.name, &lc.extras);
    }

    for line in &net.lines {
        let id = mrid_of(&line.extras, "line", &line.name);
        doc.named("ACLineSegment", &id, &line.name);
        doc.text("Conductor.length", line.length);
        if let Some(lc) = net.linecode(&line.linecode) {
            doc.reference(
                "ACLineSegment.PerLengthImpedance",
                &mrid_of(&lc.extras, "plpi", &lc.name),
            );
        } else {
            doc.warnings.push(format!(
                "line {}: linecode `{}` is not defined; the segment is written \
                 without impedance",
                line.name, line.linecode
            ));
        }
        doc.close("ACLineSegment");
        for (seq, terminal_id) in [
            (1usize, &line.terminal_map_from),
            (2, &line.terminal_map_to),
        ] {
            let bus = if seq == 1 {
                &line.bus_from
            } else {
                &line.bus_to
            };
            doc.terminal(&id, seq, &node_id(net, bus), Some(&phase_code(terminal_id)));
        }
        for (n, phase) in phases_of(&line.terminal_map_from).iter().enumerate() {
            let pid = det_mrid("linephase", &format!("{}:{phase}", line.name));
            doc.named(
                "ACLineSegmentPhase",
                &pid,
                &format!("{}_{phase}", line.name),
            );
            doc.reference("ACLineSegmentPhase.ACLineSegment", &id);
            doc.enumeration(
                "ACLineSegmentPhase.phase",
                &format!("SinglePhaseKind.{phase}"),
            );
            doc.text("ACLineSegmentPhase.sequenceNumber", n + 1);
            doc.close("ACLineSegmentPhase");
        }
        if line.terminal_map_from != line.terminal_map_to {
            doc.warnings.push(format!(
                "line {}: from/to terminal maps differ; CIM phases are written \
                 from the from side",
                line.name
            ));
        }
        doc.warn_extras("line", &line.name, &line.extras);
    }

    for load in &net.loads {
        let id = mrid_of(&load.extras, "load", &load.name);
        doc.named("EnergyConsumer", &id, &load.name);
        let connection = match load.configuration {
            Configuration::Delta => "D",
            Configuration::SinglePhase => "I",
            Configuration::Wye => "Y",
        };
        doc.enumeration(
            "EnergyConsumer.phaseConnection",
            &format!("PhaseShuntConnectionKind.{connection}"),
        );
        doc.close("EnergyConsumer");
        doc.terminal(
            &id,
            1,
            &node_id(net, &load.bus),
            Some(&phase_code(&load.terminal_map)),
        );
        for (n, phase) in phases_of(&load.terminal_map).iter().enumerate() {
            let pid = det_mrid("loadphase", &format!("{}:{phase}", load.name));
            doc.named(
                "EnergyConsumerPhase",
                &pid,
                &format!("{}_{phase}", load.name),
            );
            doc.reference("EnergyConsumerPhase.EnergyConsumer", &id);
            doc.enumeration(
                "EnergyConsumerPhase.phase",
                &format!("SinglePhaseKind.{phase}"),
            );
            doc.text(
                "EnergyConsumerPhase.p",
                load.p_nom.get(n).copied().unwrap_or(0.0),
            );
            doc.text(
                "EnergyConsumerPhase.q",
                load.q_nom.get(n).copied().unwrap_or(0.0),
            );
            doc.close("EnergyConsumerPhase");
        }
        if !matches!(
            load.voltage_model,
            crate::model::DistLoadVoltageModel::ConstantPower { .. }
        ) {
            doc.warnings.push(format!(
                "load {}: voltage-dependence model is not written (no \
                 LoadResponseCharacteristic mapping yet)",
                load.name
            ));
        }
        doc.warn_extras("load", &load.name, &load.extras);
    }

    for shunt in &net.shunts {
        let id = mrid_of(&shunt.extras, "shunt", &shunt.name);
        doc.named("LinearShuntCompensator", &id, &shunt.name);
        doc.close("LinearShuntCompensator");
        doc.terminal(
            &id,
            1,
            &node_id(net, &shunt.bus),
            Some(&phase_code(&shunt.terminal_map)),
        );
        for (n, phase) in phases_of(&shunt.terminal_map).iter().enumerate() {
            let pid = det_mrid("shuntphase", &format!("{}:{phase}", shunt.name));
            doc.named(
                "ShuntCompensatorPhase",
                &pid,
                &format!("{}_{phase}", shunt.name),
            );
            doc.reference("ShuntCompensatorPhase.ShuntCompensator", &id);
            doc.enumeration(
                "ShuntCompensatorPhase.phase",
                &format!("SinglePhaseKind.{phase}"),
            );
            let diag = |m: &Mat| m.get(n).and_then(|r| r.get(n)).copied().unwrap_or(0.0);
            doc.text("ShuntCompensatorPhase.bPerSection", diag(&shunt.b));
            doc.text("ShuntCompensatorPhase.gPerSection", diag(&shunt.g));
            doc.close("ShuntCompensatorPhase");
        }
        let off_diagonal = shunt
            .b
            .iter()
            .enumerate()
            .any(|(i, row)| row.iter().enumerate().any(|(j, v)| i != j && *v != 0.0));
        if off_diagonal {
            doc.warnings.push(format!(
                "shunt {}: off-diagonal admittance has no per-phase CIM slot; \
                 the diagonal is written",
                shunt.name
            ));
        }
        doc.warn_extras("shunt", &shunt.name, &shunt.extras);
    }

    for switch in &net.switches {
        let id = mrid_of(&switch.extras, "switch", &switch.name);
        doc.named("LoadBreakSwitch", &id, &switch.name);
        doc.text("Switch.normalOpen", switch.open);
        doc.text("Switch.open", switch.open);
        doc.close("LoadBreakSwitch");
        doc.terminal(
            &id,
            1,
            &node_id(net, &switch.bus_from),
            Some(&phase_code(&switch.terminal_map_from)),
        );
        doc.terminal(
            &id,
            2,
            &node_id(net, &switch.bus_to),
            Some(&phase_code(&switch.terminal_map_to)),
        );
        doc.warn_extras("switch", &switch.name, &switch.extras);
    }

    for xf in &net.transformers {
        let id = mrid_of(&xf.extras, "xf", &xf.name);
        if xf.windings.len() != 2 {
            doc.warnings.push(format!(
                "transformer {}: {} windings (only two-winding transformers are \
                 written); dropped",
                xf.name,
                xf.windings.len()
            ));
            continue;
        }
        doc.named("PowerTransformer", &id, &xf.name);
        doc.close("PowerTransformer");
        for (endno, winding) in xf.windings.iter().enumerate() {
            let term = det_mrid("terminal", &format!("{id}:{}", endno + 1));
            let _ = &term;
            doc.terminal(
                &id,
                endno + 1,
                &node_id(net, &winding.bus),
                Some(&phase_code(&winding.terminal_map)),
            );
            let end = det_mrid("xfend", &format!("{}:{}", xf.name, endno + 1));
            doc.named(
                "PowerTransformerEnd",
                &end,
                &format!("{}_end{}", xf.name, endno + 1),
            );
            doc.reference("PowerTransformerEnd.PowerTransformer", &id);
            doc.text("TransformerEnd.endNumber", endno + 1);
            doc.reference(
                "TransformerEnd.Terminal",
                &det_mrid("terminal", &format!("{id}:{}", endno + 1)),
            );
            doc.text("PowerTransformerEnd.ratedU", winding.v_ref);
            doc.text("PowerTransformerEnd.ratedS", winding.s_rating);
            let kind = match winding.conn {
                WindingConn::Delta => "D",
                WindingConn::Wye => "Yn",
            };
            doc.enumeration(
                "PowerTransformerEnd.connectionKind",
                &format!("WindingConnection.{kind}"),
            );
            let z_base = if winding.v_ref > 0.0 && winding.s_rating > 0.0 {
                winding.v_ref * winding.v_ref / winding.s_rating
            } else {
                0.0
            };
            doc.text("PowerTransformerEnd.r", winding.r_pct / 100.0 * z_base);
            // The pair reactance goes on end 1 (the reader sums end x's).
            let x = if endno == 0 {
                xf.xsc_pct.first().copied().unwrap_or(0.0) / 100.0 * z_base
            } else {
                0.0
            };
            doc.text("PowerTransformerEnd.x", x);
            doc.close("PowerTransformerEnd");
            if (winding.tap - 1.0).abs() > 1e-12 {
                doc.warnings.push(format!(
                    "transformer {} winding {}: off-nominal tap {} is not written \
                     (no RatioTapChanger mapping yet)",
                    xf.name,
                    endno + 1,
                    winding.tap
                ));
            }
            if winding.r_neutral.is_some() || winding.x_neutral.is_some() {
                doc.warnings.push(format!(
                    "transformer {} winding {}: neutral impedance has no CIM slot",
                    xf.name,
                    endno + 1
                ));
            }
        }
        doc.warn_extras("transformer", &xf.name, &xf.extras);
    }

    for source in &net.sources {
        let id = mrid_of(&source.extras, "source", &source.name);
        let phases = phases_of(&source.terminal_map);
        let v_ln = source.v_magnitude.first().copied().unwrap_or(0.0);
        let v_ll = if phases.len() >= 2 {
            v_ln * 3f64.sqrt()
        } else {
            v_ln
        };
        doc.named("EnergySource", &id, &source.name);
        doc.text("EnergySource.voltageMagnitude", v_ll);
        doc.text(
            "EnergySource.voltageAngle",
            source.v_angle.first().copied().unwrap_or(0.0),
        );
        doc.text("EnergySource.nominalVoltage", v_ll);
        doc.close("EnergySource");
        doc.terminal(
            &id,
            1,
            &node_id(net, &source.bus),
            Some(&phase_code(&source.terminal_map)),
        );
        doc.warn_extras("source", &source.name, &source.extras);
    }

    for (what, count) in [
        ("generator", net.generators.len()),
        ("inverter-based resource", net.ibrs.len()),
        ("control profile", net.control_profiles.len()),
        ("untyped object", net.untyped.len()),
    ] {
        if count > 0 {
            doc.warnings.push(format!(
                "{count} {what}(s) have no distribution CIM mapping yet and are \
                 dropped"
            ));
        }
    }

    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        out,
        "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"\n         \
         xmlns:cim=\"{CIM_NS}\">"
    );
    out.push_str(&doc.body);
    out.push_str("</rdf:RDF>\n");
    Conversion {
        text: out,
        sidecars: Vec::new(),
        warnings: doc.warnings,
        diagnostics: Vec::new(),
    }
}
