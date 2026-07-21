//! DGS multiconductor reader: tiered line impedance (explicit matrix,
//! Fortescue sequence, geometry-only), phase-pinned single-phase loads,
//! transformer vector groups, the ElmXnet voltage source, and the three
//! distribution targets.

use std::path::PathBuf;

use powerio_dist::{
    Configuration, DistNetwork, DistTargetFormat, WindingConn, parse_dgs_file, parse_dgs_str,
};

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/data/dist/dgs")
        .join(rel)
}

/// The transmission-side DGS fixtures (rejection cases shared by both readers).
fn shared(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/data/dgs")
        .join(rel)
}

fn feeder() -> DistNetwork {
    parse_dgs_file(fixture("feeder_3ph_v7.dgs")).expect("feeder fixture parses")
}

fn approx(a: f64, b: f64, tol: f64, what: &str) {
    assert!((a - b).abs() <= tol, "{what}: {a} vs {b}");
}

fn has_warning(net: &DistNetwork, needle: &str) {
    assert!(
        net.warnings.iter().any(|w| w.contains(needle)),
        "missing warning `{needle}` in {:?}",
        net.warnings
    );
}

#[test]
fn feeder_topology_and_counts() {
    let net = feeder();
    // Src, Mid (MidB fused into it through the closed coupler), Lv.
    assert_eq!(net.buses.len(), 3, "buses: {:?}", net.buses);
    assert_eq!(net.lines.len(), 1);
    assert_eq!(net.transformers.len(), 1);
    assert_eq!(net.loads.len(), 2);
    assert_eq!(net.shunts.len(), 1);
    assert_eq!(net.sources.len(), 1);
    assert!(
        net.switches.is_empty(),
        "closed coupler fuses, not switches"
    );
    approx(net.base_frequency, 50.0, 1e-12, "frnom");
    has_warning(&net, "fused 1 terminal pair");
    has_warning(&net, "ElmFoobar");
}

#[test]
fn sequence_linecode_is_fortescue_per_meter() {
    let net = feeder();
    let lc = &net.linecodes[0];
    assert_eq!(lc.n_conductors, 3);
    // rline 0.2, rline0 0.5 ohm/km: self (0.5 + 2*0.2)/3 / 1000 = 3e-4 ohm/m,
    // mutual (0.5 - 0.2)/3 / 1000 = 1e-4. xline 0.3, xline0 0.9: self 5e-4,
    // mutual 2e-4.
    approx(lc.r_series[0][0], 3e-4, 1e-15, "r self");
    approx(lc.r_series[0][1], 1e-4, 1e-15, "r mutual");
    approx(lc.x_series[1][1], 5e-4, 1e-15, "x self");
    approx(lc.x_series[2][0], 2e-4, 1e-15, "x mutual");
    // cline 0.01, cline0 0.005 uF/km at 50 Hz; B = 2*pi*50*C. Half at each
    // end: self (B0 + 2*B1)/3 / 2, mutual (B0 - B1)/3 / 2 per metre.
    let b1 = 2.0 * std::f64::consts::PI * 50.0 * 0.01e-6 / 1000.0;
    let b0 = 2.0 * std::f64::consts::PI * 50.0 * 0.005e-6 / 1000.0;
    approx(
        lc.b_from[0][0],
        (b0 + 2.0 * b1) / 3.0 / 2.0,
        1e-18,
        "b self",
    );
    approx(lc.b_from[0][1], (b0 - b1) / 3.0 / 2.0, 1e-18, "b mutual");
    // The line is 2.5 km.
    approx(net.lines[0].length, 2500.0, 1e-9, "length m");
    has_warning(&net, "transposed 3x3");
}

#[test]
fn single_phase_load_is_pinned_to_phase_2() {
    let net = feeder();
    let lv = net.loads.iter().find(|l| l.name == "LodLv1").unwrap();
    assert_eq!(lv.configuration, Configuration::SinglePhase);
    // StaCubic cPhInfo = L2 pins conductor 2; the wye return is the neutral.
    assert_eq!(lv.terminal_map, vec!["2".to_string(), "4".to_string()]);
    // 0.012 MW / 0.004 Mvar as watts and vars on the one phase.
    approx(lv.p_nom[0], 12_000.0, 1e-9, "p W");
    approx(lv.q_nom[0], 4_000.0, 1e-9, "q var");
    // No defaulted-phase warning: the phase was pinned.
    assert!(
        !net.warnings
            .iter()
            .any(|w| w.contains("defaulted to phase"))
    );

    let three = net.loads.iter().find(|l| l.name == "Lod1").unwrap();
    assert_eq!(three.configuration, Configuration::Wye);
    approx(
        three.p_nom.iter().sum::<f64>(),
        300_000.0,
        1e-9,
        "3ph P sum",
    );
}

#[test]
fn transformer_vector_group_and_short_circuit() {
    let net = feeder();
    let tr = &net.transformers[0];
    assert_eq!(tr.windings[0].conn, WindingConn::Delta, "HV side of Dyn");
    assert_eq!(tr.windings[1].conn, WindingConn::Wye);
    approx(tr.windings[0].v_ref, 20_000.0, 1e-9, "HV v_ref");
    approx(tr.windings[1].v_ref, 400.0, 1e-9, "LV v_ref");
    approx(tr.windings[0].s_rating, 630_000.0, 1e-6, "s_rating VA");
    // pcutr 6.5 kW on 0.63 MVA: total r% = 6.5/(10*0.63) = 1.031746...,
    // split per winding = 0.515873; xhl = sqrt(4^2 - 1.031746^2) = 3.864647.
    approx(
        tr.windings[0].r_pct,
        6.5 / (10.0 * 0.63) / 2.0,
        1e-9,
        "r_pct",
    );
    let r_total: f64 = 6.5 / (10.0 * 0.63);
    approx(
        tr.xsc_pct[0],
        (16.0 - r_total * r_total).sqrt(),
        1e-9,
        "xhl %",
    );
}

#[test]
fn xnet_becomes_the_single_voltage_source() {
    let net = feeder();
    let src = &net.sources[0];
    // 20 kV bus, usetp 1.02: phase-to-neutral 20000/sqrt(3)*1.02 volts.
    let vpn = 20_000.0 / 3f64.sqrt() * 1.02;
    approx(src.v_magnitude[0], vpn, 1e-6, "phase a volts");
    approx(src.v_magnitude[1], vpn, 1e-6, "phase b volts");
    // Angles are radians, 0 / -120 / +120 degrees.
    approx(src.v_angle[0], 0.0, 1e-12, "phase a angle");
    approx(
        src.v_angle[1],
        -2.0 * std::f64::consts::FRAC_PI_3,
        1e-12,
        "phase b angle",
    );
}

#[test]
fn explicit_matrix_is_preserved_verbatim_and_forms_agree() {
    let denorm = parse_dgs_file(fixture("tower_4wire_v7.dgs")).unwrap();
    let norm = parse_dgs_file(fixture("tower_matrix_table_v7.dgs")).unwrap();
    for net in [&denorm, &norm] {
        let lc = &net.linecodes[0];
        assert_eq!(lc.n_conductors, 4, "neutral conductor kept");
        // Totals over the 1 km section, per metre: R[0][0] 0.11/1000,
        // R[3][3] 0.25/1000, R[0][3] 0.041/1000; X completed symmetric
        // from the lower triangle in the normalized form.
        approx(lc.r_series[0][0], 0.11e-3, 1e-15, "r aa");
        approx(lc.r_series[3][3], 0.25e-3, 1e-15, "r nn");
        approx(lc.r_series[0][3], 0.041e-3, 1e-15, "r an");
        approx(lc.x_series[0][1], 0.09e-3, 1e-15, "x ab");
        approx(lc.x_series[1][0], 0.09e-3, 1e-15, "x ba (mirrored)");
        has_warning(net, "explicit 4-conductor phase matrix");
    }
    // The denormalized and normalized carriers yield the same matrices.
    assert_eq!(
        denorm.linecodes[0].r_series, norm.linecodes[0].r_series,
        "denormalized and $$Matrix forms agree"
    );
    assert_eq!(denorm.linecodes[0].x_series, norm.linecodes[0].x_series);
}

#[test]
fn geometry_without_parameters_is_skipped_with_guidance() {
    let net = parse_dgs_file(fixture("tower_geometry_only_v7.dgs")).unwrap();
    assert!(net.lines.is_empty(), "geometry-only line must not be faked");
    has_warning(&net, "no exported line parameters");
    has_warning(&net, "re-export from PowerFactory");
}

#[test]
fn rejections_match_the_transmission_reader() {
    let err = parse_dgs_file(shared("binary_reject.pfd")).unwrap_err();
    assert!(
        err.to_string().contains("encrypted PowerFactory export"),
        "{err}"
    );
    let err = parse_dgs_file(shared("opd_only_v7.dgs")).unwrap_err();
    assert!(err.to_string().contains("operational updates"), "{err}");
    let err = parse_dgs_str("$$ElmTerm;FID(a:40);loc_name(a:40)\nA;A\n").unwrap_err();
    assert!(err.to_string().contains("General"), "{err}");
}

#[test]
fn feeder_writes_all_three_distribution_targets() {
    let net = feeder();

    let dss = net.to_format(DistTargetFormat::Dss);
    assert!(dss.text.contains("New Circuit."));
    assert!(
        dss.text.contains("phases=1 conn=wye"),
        "single-phase load survives to dss"
    );
    // The written deck parses back with the same element counts.
    let back = powerio_dist::parse_dss_str(&dss.text);
    assert_eq!(back.loads.len(), net.loads.len());
    assert_eq!(back.transformers.len(), net.transformers.len());

    let pmd = net.to_format(DistTargetFormat::PmdJson);
    let doc: serde_json::Value = serde_json::from_str(&pmd.text).unwrap();
    assert_eq!(doc["data_model"], "ENGINEERING");
    assert_eq!(doc["bus"].as_object().unwrap().len(), 3);

    let bmopf = net.to_format(DistTargetFormat::BmopfJson);
    let doc: serde_json::Value = serde_json::from_str(&bmopf.text).unwrap();
    assert!(doc.get("bus").is_some());
    // Exactly one voltage source reached the BMOPF document.
    let schema: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../tests/data/dist/bmopf/draft_bmopf_schema.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let validator = jsonschema::validator_for(&schema).expect("vendored schema compiles");
    let doc: serde_json::Value = serde_json::from_str(&bmopf.text).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(&doc)
        .map(|e| format!("{}: {e}", e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "BMOPF schema violations: {errors:?}");
}

// ---- Reference smoke test (opt-in via POWERIO_DGS_REFERENCE) ----------------
//
// PowerFactory ships official DGS example exports that cannot be vendored
// here. Point POWERIO_DGS_REFERENCE at one locally to exercise the reader
// against a real export.

#[test]
#[ignore = "reads the DGS file named by POWERIO_DGS_REFERENCE; run manually"]
fn dgs_reference_smoke() {
    let Ok(reference) = std::env::var("POWERIO_DGS_REFERENCE") else {
        eprintln!("POWERIO_DGS_REFERENCE not set; skipping");
        return;
    };
    let path = std::path::Path::new(&reference);
    if !path.exists() {
        eprintln!("reference file absent; skipping");
        return;
    }
    let net = parse_dgs_file(path).unwrap();
    assert!(!net.buses.is_empty(), "reference parsed with buses");
    eprintln!(
        "{reference}: {} buses, {} lines, {} transformers, {} loads, {} sources",
        net.buses.len(),
        net.lines.len(),
        net.transformers.len(),
        net.loads.len(),
        net.sources.len()
    );
}
