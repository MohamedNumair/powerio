//! Distribution CIM (IEC 61968-13 / GridAPPS-D) import, pinned on a
//! hand-computed micro feeder, and the payoff: a CIM feeder converts to
//! OpenDSS, PMD, and BMOPF through the shared distribution machinery.

use std::path::PathBuf;

use powerio_dist::{
    Configuration, DistTargetFormat, WindingConn, parse_cim_file, parse_file, parse_str,
};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/data/dist/cim/micro_feeder.xml")
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

#[test]
#[allow(clippy::float_cmp)] // exact hand-computed fixture literals are the assertion
fn micro_feeder_maps_exactly() {
    let net = parse_cim_file(fixture()).unwrap();
    assert_eq!(net.source_format, Some(powerio_dist::DistSourceFormat::Cim));
    assert_eq!(net.base_frequency, 60.0);

    // Four connectivity nodes → four buses; the wye-secondary node carries a
    // grounded neutral terminal.
    assert_eq!(net.buses.len(), 4);
    let n4 = net.bus("n4").expect("n4 present");
    assert!(n4.terminals.contains(&"4".to_string()));
    assert_eq!(n4.grounded, vec!["4".to_string()]);

    // Line + its own linecode; impedance stays in ohm/m, length in meters.
    assert_eq!(net.lines.len(), 1);
    let line = &net.lines[0];
    assert_eq!(line.bus_from, "sourcebus");
    assert_eq!(line.bus_to, "n2");
    assert_eq!(line.terminal_map_from, ["1", "2", "3"]);
    assert!(close(line.length, 100.0));
    let lc = net.linecode(&line.linecode).expect("linecode present");
    assert_eq!(lc.n_conductors, 3);
    assert!(close(lc.r_series[0][0], 0.001), "self r/m");
    assert!(close(lc.r_series[1][0], 0.0004), "mutual r/m");
    assert!(close(lc.x_series[0][0], 0.002));
    // Shunt susceptance halves at each end.
    assert!(close(lc.b_from[0][0], 0.5e-8));
    assert!(close(lc.b_to[0][0], 0.5e-8));

    // Wye load, per-phase watts/vars straight off the phase objects.
    assert_eq!(net.loads.len(), 1);
    let load = &net.loads[0];
    assert_eq!(load.bus, "n2");
    assert_eq!(load.configuration, Configuration::Wye);
    assert_eq!(load.p_nom, vec![1000.0, 1000.0, 1000.0]);
    assert_eq!(load.q_nom, vec![500.0, 500.0, 500.0]);

    // Shunt: per-phase siemens on the diagonal.
    assert_eq!(net.shunts.len(), 1);
    assert!(close(net.shunts[0].b[0][0], 0.0001));
    assert!(close(net.shunts[0].b[1][1], 0.0001));
    assert!(close(net.shunts[0].b[0][1], 0.0));

    // Closed switch n2 -> n3.
    assert_eq!(net.switches.len(), 1);
    assert!(!net.switches[0].open);
    assert_eq!(net.switches[0].bus_from, "n2");
    assert_eq!(net.switches[0].bus_to, "n3");

    // Delta/wye transformer, impedances in percent of the winding base.
    assert_eq!(net.transformers.len(), 1);
    let xf = &net.transformers[0];
    assert_eq!(xf.windings.len(), 2);
    assert_eq!(xf.windings[0].conn, WindingConn::Delta);
    assert_eq!(xf.windings[1].conn, WindingConn::Wye);
    assert!(close(xf.windings[0].v_ref, 4160.0));
    assert!(close(xf.windings[1].v_ref, 480.0));
    let z_base = 4160.0_f64.powi(2) / 500_000.0;
    assert!(close(xf.windings[0].r_pct, 0.5 / z_base * 100.0), "r_pct");
    assert!(close(xf.xsc_pct[0], 2.0 / z_base * 100.0), "xsc_pct");

    // Source: line-to-neutral magnitude at 120-degree spacing.
    assert_eq!(net.sources.len(), 1);
    let src = &net.sources[0];
    assert_eq!(src.bus, "sourcebus");
    assert!(close(src.v_magnitude[0], 4160.0 / 3.0_f64.sqrt()));
    assert!(close(src.v_angle[1], -2.0 * std::f64::consts::PI / 3.0));
}

#[test]
fn cim_feeder_converts_to_every_distribution_format() {
    // The whole point of landing in DistNetwork: a CIM feeder now writes as
    // OpenDSS, PMD, and BMOPF with no extra bridge.
    let net = parse_cim_file(fixture()).unwrap();
    for target in [
        DistTargetFormat::Dss,
        DistTargetFormat::PmdJson,
        DistTargetFormat::BmopfJson,
    ] {
        let conv = net.to_canonical_format(target);
        assert!(!conv.text.is_empty(), "{}: empty output", target.name());
    }

    // OpenDSS output names the feeder's elements.
    let dss = net.to_canonical_format(DistTargetFormat::Dss).text;
    assert!(dss.to_lowercase().contains("line.line1"), "dss: {dss}");
    assert!(dss.to_lowercase().contains("load.load1"));
    assert!(dss.to_lowercase().contains("transformer.xfm1"));

    // PMD and BMOPF reparse: proof the conversion is structurally valid, not
    // just non-empty text.
    let pmd = net.to_canonical_format(DistTargetFormat::PmdJson).text;
    let reparsed = parse_str(&pmd, "pmd-json").unwrap();
    assert_eq!(reparsed.lines.len(), net.lines.len());
    assert_eq!(reparsed.loads.len(), net.loads.len());

    let bmopf = net.to_canonical_format(DistTargetFormat::BmopfJson).text;
    let reparsed = parse_str(&bmopf, "bmopf-json").unwrap();
    assert_eq!(reparsed.lines.len(), net.lines.len());
}

#[test]
fn xml_extension_and_cim_token_both_route() {
    // Auto-detected by extension.
    let net = parse_file(fixture(), None).unwrap();
    assert_eq!(net.buses.len(), 4);
    // Explicit token via in-memory text.
    let text = std::fs::read_to_string(fixture()).unwrap();
    let net = parse_str(&text, "cim").unwrap();
    assert_eq!(net.buses.len(), 4);
    // `cim` is a full writable distribution target.
    assert_eq!(
        powerio_dist::dist_target_from_name("cim"),
        Some(DistTargetFormat::CimXml)
    );
}

/// The writer is the reader's inverse: parse → write → parse preserves the
/// projection, imported mRIDs pass through, and a second write is byte
/// identical.
#[test]
fn cim_writer_round_trips() {
    let net = parse_cim_file(fixture()).unwrap();
    let first = net.to_canonical_format(DistTargetFormat::CimXml);
    let back = parse_str(&first.text, "cim").unwrap();

    assert_eq!(back.buses.len(), net.buses.len());
    assert_eq!(back.lines.len(), net.lines.len());
    assert_eq!(back.loads.len(), net.loads.len());
    assert_eq!(back.shunts.len(), net.shunts.len());
    assert_eq!(back.switches.len(), net.switches.len());
    assert_eq!(back.transformers.len(), net.transformers.len());
    assert_eq!(back.sources.len(), net.sources.len());

    let lc_a = net.linecode(&net.lines[0].linecode).unwrap();
    let lc_b = back.linecode(&back.lines[0].linecode).unwrap();
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
    assert_eq!(back.loads[0].p_nom, net.loads[0].p_nom);
    assert_eq!(back.loads[0].configuration, net.loads[0].configuration);
    assert!(close(back.shunts[0].b[0][0], net.shunts[0].b[0][0]));
    let (a, b) = (&net.transformers[0], &back.transformers[0]);
    assert_eq!(a.windings[0].conn, b.windings[0].conn);
    assert!(close(a.windings[0].v_ref, b.windings[0].v_ref));
    assert!(
        close(a.windings[0].r_pct, b.windings[0].r_pct),
        "r_pct {} vs {}",
        a.windings[0].r_pct,
        b.windings[0].r_pct
    );
    assert!(
        close(a.xsc_pct[0], b.xsc_pct[0]),
        "xsc {} vs {}",
        a.xsc_pct[0],
        b.xsc_pct[0]
    );
    assert!(close(
        net.sources[0].v_magnitude[0],
        back.sources[0].v_magnitude[0]
    ));

    // Deterministic ids: the reparse carries the same mRIDs, so a second
    // write is byte identical.
    let second = back.to_canonical_format(DistTargetFormat::CimXml);
    assert_eq!(first.text, second.text);

    // Same-format echo tier: a single-document parse retains its source.
    assert_eq!(back.to_format(DistTargetFormat::CimXml).text, first.text);
}

/// dss → CIM: any distribution source now writes CIM.
#[test]
fn dss_converts_to_cim() {
    let dss = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/data/dist/micro/xfmr_single_phase.dss");
    let net = parse_file(dss, None).unwrap();
    let conv = net.to_canonical_format(DistTargetFormat::CimXml);
    let back = parse_str(&conv.text, "cim").unwrap();
    assert_eq!(back.transformers.len(), net.transformers.len());
    assert_eq!(back.loads.len(), net.loads.len());
}
