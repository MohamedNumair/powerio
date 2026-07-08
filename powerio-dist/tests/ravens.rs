//! Multiconductor MG-RAVENS import/export, pinned on the upstream
//! `case3_balanced` example, plus the payoff: a RAVENS feeder converts to
//! OpenDSS, PMD, and BMOPF, and every distribution source converts to RAVENS.

use std::path::PathBuf;

use powerio_dist::{
    Configuration, DistSourceFormat, DistTargetFormat, parse_file, parse_ravens_file,
    parse_ravens_str, parse_str,
};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/data/dist/ravens/case3_balanced.json")
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

#[test]
#[allow(clippy::float_cmp)] // exact hand-checked fixture literals are the assertion
fn case3_balanced_maps_exactly() {
    let net = parse_ravens_file(fixture()).unwrap();
    assert_eq!(net.source_format, Some(DistSourceFormat::RavensJson));
    // MG-RAVENS carries no frequency; the reader assumes the OpenDSS default.
    assert_eq!(net.base_frequency, 60.0);

    // Three connectivity nodes; the source and the wye-load node carry a
    // grounded neutral terminal, the transit node does not.
    assert_eq!(net.buses.len(), 3);
    let loadbus = net.bus("loadbus").expect("loadbus present");
    assert!(loadbus.terminals.contains(&"4".to_string()));
    assert_eq!(loadbus.grounded, vec!["4".to_string()]);
    assert!(net.bus("primary").unwrap().grounded.is_empty());

    // Two per-length impedance matrices → two linecodes, ohm/m, the shunt
    // susceptance split in half at each end.
    assert_eq!(net.linecodes.len(), 2);
    let lc = net.linecode("556mcm").expect("556mcm present");
    assert_eq!(lc.n_conductors, 3);
    assert!(close(lc.r_series[0][0], 0.1));
    assert!(close(lc.r_series[1][0], 0.04), "mutual r");
    assert!(close(lc.x_series[0][0], 0.0583));
    assert!(close(lc.b_from[0][0], 1.6e-5 / 2.0));
    assert!(close(lc.b_to[0][0], 1.6e-5 / 2.0));
    // The emergency current limit becomes the linecode ampacity.
    assert_eq!(lc.i_max, Some(vec![600.0, 600.0, 600.0]));

    // Two three-phase lines; impedance stays in ohm/m, length in meters.
    assert_eq!(net.lines.len(), 2);
    let ohline = net.lines.iter().find(|l| l.name == "ohline").unwrap();
    assert_eq!(ohline.bus_from, "sourcebus");
    assert_eq!(ohline.bus_to, "primary");
    assert_eq!(ohline.terminal_map_from, ["1", "2", "3"]);
    assert!(close(ohline.length, 1.0));
    assert_eq!(ohline.linecode, "556mcm");
    // The continuous limit rides in extras as the dss `normamps`.
    assert!(close(
        ohline
            .extras
            .get("normamps")
            .and_then(serde_json::Value::as_f64)
            .unwrap(),
        400.0
    ));

    // Three single-phase wye loads, one per phase, powers straight off the
    // phase objects, line-to-neutral nominal voltage off the 400 V base.
    assert_eq!(net.loads.len(), 3);
    let l1 = net.loads.iter().find(|l| l.name == "l1").unwrap();
    assert_eq!(l1.bus, "loadbus");
    assert_eq!(l1.configuration, Configuration::SinglePhase);
    assert_eq!(l1.terminal_map, ["1", "4"]);
    assert_eq!(l1.p_nom, vec![6000.0]);
    assert_eq!(l1.q_nom, vec![3000.0]);
    assert!(close(l1.voltage_model.v_nom()[0], 400.0 / 3f64.sqrt()));

    // The Thevenin source: line-to-neutral magnitude at 120-degree spacing,
    // the base kV and per-unit recovered into extras.
    assert_eq!(net.sources.len(), 1);
    let src = &net.sources[0];
    assert_eq!(src.bus, "sourcebus");
    assert!(close(src.v_magnitude[0], 398.36 / 3f64.sqrt()));
    assert!(close(src.v_angle[1], -2.0 * std::f64::consts::PI / 3.0));
    assert!(close(
        src.extras
            .get("basekv")
            .and_then(serde_json::Value::as_f64)
            .unwrap(),
        0.4
    ));
    assert!(close(
        src.extras
            .get("pu")
            .and_then(serde_json::Value::as_f64)
            .unwrap(),
        0.9959
    ));

    // The load-shape schedule has no typed model and is preserved untyped so
    // it round-trips to MG-RAVENS.
    assert!(
        net.untyped
            .iter()
            .any(|u| u.class == "EnergyConsumerSchedule" && u.name == "ls1")
    );
}

#[test]
fn balanced_transmission_profile_is_rejected() {
    // An AlgorithmSettings/ProducerCostFunction document is the balanced
    // profile the `powerio` crate reads; the multiconductor reader refuses it
    // rather than inventing per-phase detail.
    let balanced = r#"{
      "ConnectivityNode": {"b": {"Ravens.cimObjectType": "ConnectivityNode",
        "IdentifiedObject.name": "b"}},
      "ApplicationSettings": {"s": {"Ravens.cimObjectType": "AlgorithmSettings",
        "IdentifiedObject.name": "s"}}
    }"#;
    let err = parse_ravens_str(balanced).unwrap_err();
    assert!(
        err.to_string().contains("balanced"),
        "expected a balanced-profile rejection, got: {err}"
    );
}

#[test]
fn ravens_feeder_converts_to_every_distribution_format() {
    // The whole point of landing in DistNetwork: a RAVENS feeder now writes
    // as OpenDSS, PMD, and BMOPF with no extra bridge.
    let net = parse_ravens_file(fixture()).unwrap();
    for target in [
        DistTargetFormat::Dss,
        DistTargetFormat::PmdJson,
        DistTargetFormat::BmopfJson,
    ] {
        let conv = net.to_canonical_format(target);
        assert!(!conv.text.is_empty(), "{}: empty output", target.name());
    }

    // OpenDSS output names the feeder's elements.
    let dss = net
        .to_canonical_format(DistTargetFormat::Dss)
        .text
        .to_lowercase();
    assert!(dss.contains("line.ohline"), "dss: {dss}");
    assert!(dss.contains("load.l1"));
    assert!(dss.contains("linecode.556mcm"));

    // PMD and BMOPF reparse: proof the conversion is structurally valid.
    let pmd = net.to_canonical_format(DistTargetFormat::PmdJson).text;
    let reparsed = parse_str(&pmd, "pmd-json").unwrap();
    assert_eq!(reparsed.lines.len(), net.lines.len());
    assert_eq!(reparsed.loads.len(), net.loads.len());

    let bmopf = net.to_canonical_format(DistTargetFormat::BmopfJson).text;
    let reparsed = parse_str(&bmopf, "bmopf-json").unwrap();
    assert_eq!(reparsed.lines.len(), net.lines.len());
}

#[test]
fn json_extension_and_ravens_token_both_route() {
    // Auto-detected by the top-level RAVENS markers.
    let net = parse_file(fixture(), None).unwrap();
    assert_eq!(net.loads.len(), 3);
    // Explicit token via in-memory text.
    let text = std::fs::read_to_string(fixture()).unwrap();
    let net = parse_str(&text, "ravens").unwrap();
    assert_eq!(net.loads.len(), 3);
    // `ravens` is a full writable distribution target.
    assert_eq!(
        powerio_dist::dist_target_from_name("mgravens"),
        Some(DistTargetFormat::RavensJson)
    );
}

/// The writer is the reader's inverse: parse → write → parse preserves the
/// projection, imported mRIDs pass through, and a second write is byte
/// identical.
#[test]
fn ravens_writer_round_trips() {
    let net = parse_ravens_file(fixture()).unwrap();
    let first = net.to_canonical_format(DistTargetFormat::RavensJson);
    let back = parse_ravens_str(&first.text).unwrap();

    assert_eq!(back.buses.len(), net.buses.len());
    assert_eq!(back.linecodes.len(), net.linecodes.len());
    assert_eq!(back.lines.len(), net.lines.len());
    assert_eq!(back.loads.len(), net.loads.len());
    assert_eq!(back.sources.len(), net.sources.len());

    let lc_a = net.linecode("556mcm").unwrap();
    let lc_b = back.linecode("556mcm").unwrap();
    for i in 0..lc_a.n_conductors {
        for j in 0..lc_a.n_conductors {
            assert!(close(lc_a.r_series[i][j], lc_b.r_series[i][j]), "r {i}{j}");
            assert!(close(lc_a.x_series[i][j], lc_b.x_series[i][j]), "x {i}{j}");
            assert!(
                close(
                    lc_a.b_from[i][j] + lc_a.b_to[i][j],
                    lc_b.b_from[i][j] + lc_b.b_to[i][j]
                ),
                "b {i}{j}"
            );
        }
    }
    let la = net.loads.iter().find(|l| l.name == "l1").unwrap();
    let lb = back.loads.iter().find(|l| l.name == "l1").unwrap();
    assert_eq!(la.p_nom, lb.p_nom);
    assert_eq!(la.configuration, lb.configuration);
    assert!(close(
        net.sources[0].v_magnitude[0],
        back.sources[0].v_magnitude[0]
    ));
    assert!(close(net.sources[0].v_angle[1], back.sources[0].v_angle[1]));

    // Imported mRIDs pass through, so the second write is byte identical.
    let second = back.to_canonical_format(DistTargetFormat::RavensJson);
    assert_eq!(first.text, second.text);

    // Same-format echo tier: a single-document parse retains its source, so
    // the original network echoes the original fixture bytes, and the
    // reparse of the canonical output echoes that canonical output.
    let original = std::fs::read_to_string(fixture()).unwrap();
    assert_eq!(net.to_format(DistTargetFormat::RavensJson).text, original);
    assert_eq!(
        back.to_format(DistTargetFormat::RavensJson).text,
        first.text
    );
}

/// dss → RAVENS: any distribution source now writes RAVENS, including a
/// transformer, which round-trips through the catalog `TransformerEndInfo`
/// records.
#[test]
fn dss_converts_to_ravens_with_transformer() {
    let dss = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/data/dist/micro/xfmr_single_phase.dss");
    let net = parse_file(dss, None).unwrap();
    let conv = net.to_canonical_format(DistTargetFormat::RavensJson);
    let back = parse_ravens_str(&conv.text).unwrap();
    assert_eq!(back.transformers.len(), net.transformers.len());
    assert_eq!(back.loads.len(), net.loads.len());
    assert_eq!(back.sources.len(), net.sources.len());

    // The transformer winding voltages and connections survive the trip
    // through the RAVENS catalog records.
    if let (Some(a), Some(b)) = (net.transformers.first(), back.transformers.first()) {
        assert_eq!(a.windings.len(), b.windings.len());
        for (wa, wb) in a.windings.iter().zip(&b.windings) {
            assert_eq!(wa.conn, wb.conn, "winding connection");
            assert!(
                close(wa.v_ref, wb.v_ref),
                "winding voltage {} vs {}",
                wa.v_ref,
                wb.v_ref
            );
        }
    }
}

/// A pmd feeder also converts to RAVENS and reparses, exercising the ZIP /
/// exponential load-response path through a richer source.
#[test]
fn dss_zip_load_round_trips_through_ravens() {
    let dss = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/data/dist/micro/onephase_zip_load.dss");
    let net = parse_file(dss, None).unwrap();
    let conv = net.to_canonical_format(DistTargetFormat::RavensJson);
    let back = parse_ravens_str(&conv.text).unwrap();
    assert_eq!(back.loads.len(), net.loads.len());
    // Writing the reparse again is byte identical (deterministic mRIDs).
    let second = back.to_canonical_format(DistTargetFormat::RavensJson);
    assert_eq!(conv.text, second.text);
}
