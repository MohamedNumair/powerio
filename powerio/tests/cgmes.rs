//! CGMES integration: the hand-computed CGMES 3.0 micro case, the vendored
//! CIGRE MV set (CGMES 2.4.15, vendor export without SSH), and the
//! node-breaker switches sample, all through the hub's directory dispatch.

use std::path::PathBuf;

use powerio::{BusType, parse_file};

fn data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/data/cgmes")
        .join(name)
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

/// Every expected value here is hand-computed from the fixture's numbers;
/// the table lives in tests/data/cgmes/README.md.
#[test]
#[allow(clippy::float_cmp)] // exact fixture literals are the assertion
fn micro30_maps_exactly() {
    let parsed = parse_file(data("micro30"), None).unwrap();
    let net = parsed.network;
    assert_eq!(net.base_mva, 100.0);
    assert_eq!(net.base_frequency, 50.0);

    assert_eq!(net.buses.len(), 3);
    let b1 = &net.buses[0];
    assert_eq!(b1.name.as_deref(), Some("B1"));
    assert_eq!(b1.base_kv, 110.0);
    assert_eq!(b1.kind, BusType::Ref, "island angle reference");
    assert!(close(b1.vm, 112.2 / 110.0));
    assert!(close(b1.va, 0.0));
    let b3 = &net.buses[2];
    assert_eq!(b3.base_kv, 20.0);
    assert!(close(b3.vm, 20.5 / 20.0));
    assert!(close(b3.va, -4.2));

    // Line: ohms on the 110 kV / 100 MVA base (z_base 121 Ω).
    assert_eq!(net.branches.len(), 2);
    let line = &net.branches[0];
    assert!(close(line.r, 2.42 / 121.0), "r {}", line.r);
    assert!(close(line.x, 12.1 / 121.0));
    assert!(close(line.b, 3e-6 * 121.0));
    // Current limits: √3·kV·A/1000, patl → A, tatl → B.
    assert!(close(line.rate_a, 3f64.sqrt() * 110.0 * 400.0 / 1000.0));
    assert!(close(line.rate_b, 3f64.sqrt() * 110.0 * 500.0 / 1000.0));

    // Transformer: end1 impedance on its rated base, ratio from rated
    // voltages over bus bases times the tap step (step 15, neutral 13,
    // 1.25 %/step on end 2 inverts).
    let xf = &net.branches[1];
    assert!(close(xf.r, 0.6 / 121.0));
    assert!(close(xf.x, 10.0 / 121.0));
    let base_ratio = (110.0 / 110.0) / (21.0 / 20.0);
    let tap_factor = 1.0 + (15.0 - 13.0) * 1.25 / 100.0;
    assert!(close(xf.tap, base_ratio / tap_factor), "tap {}", xf.tap);

    assert_eq!(net.loads.len(), 1);
    assert!(close(net.loads[0].p, 20.0));
    assert!(close(net.loads[0].q, 5.0));

    assert_eq!(net.generators.len(), 1);
    let machine = &net.generators[0];
    assert!(close(machine.pg, 45.0), "CIM injection sign flips");
    assert!(close(machine.qg, 12.0));
    assert!(close(machine.qmax, 25.0));
    assert!(close(machine.qmin, -20.0));
    assert!(close(machine.pmax, 80.0));
    assert!(close(machine.mbase, 50.0));
    assert!(close(machine.vg, 115.5 / 110.0), "AVR target over bus base");

    // Shunt: S·kV² at 2 sections in.
    assert_eq!(net.shunts.len(), 1);
    assert!(close(net.shunts[0].b, 0.0012 * 2.0 * 400.0));

    assert_eq!(net.switches.len(), 1);
    assert!(net.switches[0].closed);
    assert_eq!(net.switches[0].current_rating, Some(3000.0));
}

#[test]
#[allow(clippy::float_cmp)] // exact fixture literals are the assertion
fn cigre_mv_vendor_set_reads_without_ssh() {
    let parsed = parse_file(data("cigre_mv"), None).unwrap();
    let net = &parsed.network;
    // 14 MV nodes plus the HV connection point.
    assert_eq!(net.buses.len(), 15);
    assert_eq!(net.loads.len(), 18);
    assert_eq!(net.branches.len(), 14);
    assert_eq!(net.generators.len(), 1);

    // The set carries no SSH; loads must fall back to SvPowerFlow, and the
    // solved voltages come from SV.
    assert!(net.loads.iter().any(|l| l.p != 0.0));
    let slack = net.buses.iter().find(|b| b.kind == BusType::Ref).unwrap();
    assert_eq!(slack.name.as_deref(), Some("N0"));
    assert_eq!(slack.base_kv, 110.0);
    assert!(net.buses.iter().all(|b| b.vm > 0.9 && b.vm < 1.1));

    // The DI (diagram) part is skipped loudly, and the 100 MVA assumption is
    // named.
    assert!(parsed.warnings.iter().any(|w| w.contains("DI.xml")));
    assert!(parsed.warnings.iter().any(|w| w.contains("100 MVA")));
}

#[test]
fn node_breaker_switch_sample_reads_switch_states() {
    let parsed = parse_file(data("sample_grid_switches"), None).unwrap();
    let net = &parsed.network;
    assert_eq!(net.buses.len(), 12);
    assert_eq!(net.switches.len(), 5);
    assert_eq!(net.branches.len(), 3);
    // A pure switching topology has no machines: the missing angle
    // reference must be loud, not silent.
    assert!(
        parsed
            .warnings
            .iter()
            .any(|w| w.contains("no angle reference")),
        "warnings: {:?}",
        parsed.warnings
    );
}

#[test]
fn cgmes_tokens_route_and_stay_read_only() {
    // Directory dispatch also honors the explicit token.
    let parsed = parse_file(data("micro30"), Some("cgmes")).unwrap();
    assert_eq!(parsed.network.buses.len(), 3);
    // Read only: no writer target resolves for the token.
    assert_eq!(powerio::target_format_from_name("cgmes"), None);
}
