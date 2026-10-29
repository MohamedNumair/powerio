//! The IEEE 13 node feeder as the upstream MG-RAVENS converter writes it
//! (`ravens/IEEE13_Assets.json`, MG-RAVENS 094ad116), against the OpenDSS
//! source it was converted from (`opendss/ieee13/IEEE13Nodeckt.dss`).

mod helpers;

use helpers::Parsed;
use powerio_dist::DistTargetFormat;
use std::path::PathBuf;

fn case(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/data/dist")
        .join(rel)
}

fn ravens() -> Parsed {
    helpers::parse_file(case("ravens/IEEE13_Assets.json"), Some("ravens-json")).unwrap()
}

fn dss() -> Parsed {
    helpers::parse_dss_file(case("opendss/ieee13/IEEE13Nodeckt.dss")).unwrap()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

/// Every transformer, the substation, the feeder transformer, and the three
/// regulator tanks, carries the OpenDSS ratings, connections, resistances and
/// leakage reactance; each regulator's tap is on its tap winding.
#[test]
fn transformers_match_the_opendss_source() {
    let (net, reference) = (ravens(), dss());
    assert_eq!(net.transformers().len(), 5, "{:?}", net.warnings);
    for t in net.transformers() {
        let d = reference
            .transformers()
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(&t.name))
            .unwrap_or_else(|| panic!("{} has no OpenDSS counterpart", t.name));
        assert_eq!(t.phases, d.phases, "{}", t.name);
        assert_eq!(t.xsc_pct.len(), d.xsc_pct.len(), "{}", t.name);
        for (x, y) in t.xsc_pct.iter().zip(&d.xsc_pct) {
            assert!(close(*x, *y), "{}: xsc {x} vs {y}", t.name);
        }
        for (k, (w, v)) in t.windings.iter().zip(&d.windings).enumerate() {
            let what = format!("{} winding {}", t.name, k + 1);
            assert!(
                w.bus.eq_ignore_ascii_case(&v.bus),
                "{what}: {} vs {}",
                w.bus,
                v.bus
            );
            assert_eq!(w.terminal_map, v.terminal_map, "{what}");
            assert_eq!(w.conn, v.conn, "{what}");
            assert!(
                close(w.v_ref, v.v_ref),
                "{what}: {} vs {}",
                w.v_ref,
                v.v_ref
            );
            assert!(close(w.s_rating, v.s_rating), "{what}");
            assert!(
                close(w.r_pct, v.r_pct),
                "{what}: {} vs {}",
                w.r_pct,
                v.r_pct
            );
        }
    }
    // The OpenDSS case leaves its regulators at tap 1 for the RegControls to
    // move; the RAVENS document states the solved step on end 2.
    for (name, tap) in [("reg1", 1.0625), ("reg2", 1.05), ("reg3", 1.06875)] {
        let t = net.transformers().iter().find(|t| t.name == name).unwrap();
        let taps: Vec<f64> = t.windings.iter().map(|w| w.tap).collect();
        assert_eq!(taps, [1.0, tap], "{name}");
    }
}

/// The fifteen loads keep their buses, phases, connections and powers.
#[test]
fn loads_match_the_opendss_source() {
    let (net, reference) = (ravens(), dss());
    assert_eq!(net.loads().len(), reference.loads().len());
    for d in reference.loads() {
        let l = net
            .loads()
            .iter()
            .find(|l| l.name.eq_ignore_ascii_case(&d.name))
            .unwrap_or_else(|| panic!("load {} missing", d.name));
        assert!(l.bus.eq_ignore_ascii_case(&d.bus), "{}", d.name);
        assert_eq!(l.configuration, d.configuration, "{}", d.name);
        let total = |v: &[f64]| v.iter().sum::<f64>();
        assert!(close(total(&l.p_nom), total(&d.p_nom)), "{}: p", d.name);
        assert!(close(total(&l.q_nom), total(&d.q_nom)), "{}: q", d.name);
    }
}

/// Ten of the eleven segments state their impedance through wire and cable
/// assets, which have no typed reading yet: each is kept untyped with its
/// own finding, never silently dropped. When asset impedances land, this
/// test is the one to turn into a comparison.
#[test]
fn asset_impedance_lines_are_named_not_dropped() {
    let net = ravens();
    let untyped: Vec<&str> = net
        .warnings
        .iter()
        .filter(|w| w.contains("READ.RAVENS_DIST.RETAINED_SOURCE_ONLY"))
        .filter(|w| w.contains("no PerLengthImpedance"))
        .map(String::as_str)
        .collect();
    let typed = net
        .lines()
        .iter()
        .filter(|l| !l.name.starts_with("_pio_"))
        .count();
    assert_eq!(typed + untyped.len(), 11, "{untyped:?}");
    assert_eq!(untyped.len(), 10, "{untyped:?}");
}

/// Same format output echoes the upstream bytes; the canonical writer is a
/// fixed point from its first output on.
#[test]
fn echo_and_canonical_fixed_point() {
    let net = ravens();
    let bytes = std::fs::read_to_string(case("ravens/IEEE13_Assets.json")).unwrap();
    assert!(
        net.emit(DistTargetFormat::RavensJson).text == bytes,
        "same format output is not the source bytes"
    );
    let first = net.emit_value(DistTargetFormat::RavensJson);
    let back = helpers::parse_str(&first.text, "ravens-json").unwrap();
    let second = back.emit_value(DistTargetFormat::RavensJson);
    assert!(
        first.text == second.text,
        "canonical RAVENS write is not stable"
    );
}
