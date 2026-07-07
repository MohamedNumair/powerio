//! DIgSILENT DGS reader/writer tests: hand-computed mapping assertions, v5/v7
//! parse-equivalence, encoding, topology fusion, error cases, a golden writer
//! output, and a MATPOWER oracle round trip.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use powerio::{
    Branch, Bus, BusId, BusType, Generator, Load, Network, parse_dgs, parse_file,
    parse_matpower_file, write_dgs,
};

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/data")
        .join(name)
}

fn fixture(name: &str) -> Network {
    parse_file(data(&format!("dgs/{name}")), None)
        .unwrap()
        .network
}

/// Parse an in-memory DGS document, returning network plus read warnings.
fn parse(text: &str) -> powerio::Parsed {
    powerio::parse_str(text, "dgs").unwrap()
}

fn approx(a: f64, b: f64, tol: f64, what: &str) {
    assert!((a - b).abs() <= tol, "{what}: {a} != {b} (tol {tol})");
}

/// A branch's compared fields: `(r, x, b, tap, shift, rate_a, is_transformer)`.
type BranchTuple = (f64, f64, f64, f64, f64, f64, bool);

fn branch(net: &Network, from: usize, to: usize) -> &Branch {
    net.branches
        .iter()
        .find(|b| b.from.0 == from && b.to.0 == to)
        .unwrap_or_else(|| panic!("no branch {from}->{to}"))
}

// ---- Fixture mapping assertions --------------------------------------------

#[test]
fn v7_fixture_maps_every_class() {
    let net = fixture("minimal_v7.dgs");
    // ElmNet: name + frnom.
    assert_eq!(net.name, "Grid");
    assert!((net.base_frequency - 50.0).abs() < 1e-9);
    assert!((net.base_mva - 100.0).abs() < 1e-9);
    // Five terminals, one absorbed by a closed coupler => four buses.
    assert_eq!(net.buses.len(), 4);
    assert_eq!(net.buses[0].kind, BusType::Ref); // HV1: ElmXnet SL
    assert_eq!(net.buses[1].kind, BusType::Pv); // HV2: ElmSym (iv_mode=1)
    assert_eq!(net.buses[2].kind, BusType::Pq);
    assert_eq!(net.buses[3].kind, BusType::Pq);
    // PV/ref bus voltage taken from the controlling machine setpoint.
    approx(net.buses[0].vm, 1.02, 1e-9, "HV1 vm=xnet usetp");
    approx(net.buses[1].vm, 1.01, 1e-9, "HV2 vm=gen usetp");
    // Fused bus keeps the absorbed terminal's name.
    let merged = net.buses[3].extras.get("dgs_merged_terms").unwrap();
    assert_eq!(merged.as_array().unwrap()[0].as_str().unwrap(), "MV3");

    // Two loads (Load2's loc_name exercises quoting: "Load;2").
    assert_eq!(net.loads.len(), 2);
    let load2 = net.loads.iter().find(|l| l.bus.0 == 4).unwrap();
    approx(load2.p, 3.0, 1e-9, "Load2 p");
    approx(load2.q, 1.5, 1e-9, "Load2 q");

    // Shunt: qtotn path, capacitor (shtype 2) => b = +ncapa*qtotn*(uknom/ushnm)^2
    //       = 1*5*(110/110)^2 = 5; g = 0.
    assert_eq!(net.shunts.len(), 1);
    approx(net.shunts[0].g, 0.0, 1e-12, "shunt g");
    approx(net.shunts[0].b, 5.0, 1e-9, "shunt b");

    // Generators: the external grid (SL) and the synchronous machine.
    assert_eq!(net.generators.len(), 2);
    let machine = net.generators.iter().find(|g| g.bus.0 == 2).unwrap();
    // pg = pgini*ngnum = 15; qg = qgini = 5; vg = usetp = 1.01;
    // mbase = sgn*ngnum = 25; pmax = sgn*cosn = 25*0.9 = 22.5;
    // qmax = q_max*sgn = 1*25 = 25; qmin = q_min*sgn = -25.
    approx(machine.pg, 15.0, 1e-9, "gen pg");
    approx(machine.qg, 5.0, 1e-9, "gen qg");
    approx(machine.vg, 1.01, 1e-9, "gen vg");
    approx(machine.mbase, 25.0, 1e-9, "gen mbase");
    approx(machine.pmax, 22.5, 1e-9, "gen pmax");
    approx(machine.qmax, 25.0, 1e-9, "gen qmax");
    approx(machine.qmin, -25.0, 1e-9, "gen qmin");
    let xnet = net.generators.iter().find(|g| g.bus.0 == 1).unwrap();
    approx(xnet.vg, 1.02, 1e-9, "xnet vg");
    approx(xnet.mbase, 100.0, 1e-9, "xnet mbase");

    // Three branches (2 lines + 1 transformer) and one open-coupler switch.
    assert_eq!(net.branches.len(), 3);
    assert_eq!(net.switches.len(), 1);
    assert!(!net.switches[0].closed);
}

#[test]
fn v7_line_section_summation_and_charging() {
    let net = fixture("minimal_v7.dgs");
    // L1 (HV1->HV2, 110 kV): two sections 4 km + 6 km of LNETYPE1
    //   (rline 0.1 Ohm/km, xline 0.4, cline 0.01 uF/km). Z_base = 110^2/100 = 121.
    //   r = 0.1*(4+6)/121 = 1.0/121; x = 0.4*10/121 = 4.0/121.
    //   b = 2*pi*50*0.01e-6*(4+6)*121.
    let l1 = branch(&net, 1, 2);
    approx(l1.r, 1.0 / 121.0, 1e-12, "L1 r");
    approx(l1.x, 4.0 / 121.0, 1e-12, "L1 x");
    let b_expect = std::f64::consts::TAU * 50.0 * 0.01e-6 * 10.0 * 121.0;
    approx(l1.b, b_expect, 1e-12, "L1 b (sum of sections)");
    // rate_a = sqrt(3)*uline*sline*fline = sqrt(3)*110*0.5*1.
    approx(l1.rate_a, 3.0_f64.sqrt() * 110.0 * 0.5, 1e-9, "L1 rate_a");
    assert!(l1.in_service);

    // L2 (MV1->MV2, 20 kV) has an open StaSwitch on its to-side cubicle => out.
    let l2 = branch(&net, 3, 4);
    assert!(!l2.in_service, "L2 open switch => out of service");
    // Z_base = 20^2/100 = 4; r = 0.2*5/4 = 0.25; x = 0.3*5/4 = 0.375.
    approx(l2.r, 0.25, 1e-12, "L2 r");
    approx(l2.x, 0.375, 1e-12, "L2 x");
}

#[test]
fn v7_transformer_tap_and_magnetizing() {
    let net = fixture("minimal_v7.dgs");
    // TR (HV2->MV1): strn=40, uktr=12%, pcutr=100 kW, curmg=0.5%, pfe=10 kW,
    // dutap=2.5%, nntap=1, nntap0=0, phitr=0, tap_side=0.
    //   z_t = 0.12; r_t = 100/(1000*40) = 0.0025; x_t = sqrt(0.12^2 - 0.0025^2).
    //   rebase x(100/40 = 2.5): r = 0.00625; x = x_t*2.5.
    let tr = branch(&net, 2, 3);
    let x_t = (0.12_f64.powi(2) - 0.0025_f64.powi(2)).sqrt();
    approx(tr.r, 0.00625, 1e-12, "TR r");
    approx(tr.x, x_t * 2.5, 1e-12, "TR x");
    // Tap: n=1, du=0.025, theta=0 => t=1.025, alpha=0 => tap=1.025, shift=0.
    approx(tr.effective_tap(), 1.025, 1e-12, "TR tap");
    approx(tr.shift, 0.0, 1e-12, "TR shift");
    // rate_a = strn*ratfac = 40*1.
    approx(tr.rate_a, 40.0, 1e-9, "TR rate_a");
    // Magnetizing => from-side charging: y_m=0.005, g_m=10/(1000*40)=0.00025,
    // b_m=-sqrt(0.005^2 - 0.00025^2); rebased x(40/100).
    let c = tr.charging.expect("magnetizing charging present");
    approx(c.g_fr, 0.00025 * 0.4, 1e-12, "TR g_fr");
    let b_m = -(0.005_f64.powi(2) - 0.00025_f64.powi(2)).sqrt();
    approx(c.b_fr, b_m * 0.4, 1e-12, "TR b_fr");
    approx(c.g_to, 0.0, 1e-15, "TR g_to");
}

#[test]
fn v5_and_v7_parse_to_the_same_network() {
    let v7 = fixture("minimal_v7.dgs");
    let v5 = fixture("minimal_v5.dgs");
    assert_eq!(v5.buses.len(), v7.buses.len());
    assert_eq!(v5.branches.len(), v7.branches.len());
    assert_eq!(v5.generators.len(), v7.generators.len());
    assert_eq!(v5.loads.len(), v7.loads.len());
    assert_eq!(v5.shunts.len(), v7.shunts.len());
    assert_eq!(v5.switches.len(), v7.switches.len());
    for (a, b) in v5.buses.iter().zip(&v7.buses) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.kind, b.kind);
        approx(a.base_kv, b.base_kv, 1e-12, "v5/v7 base_kv");
    }
    for (a, b) in v5.branches.iter().zip(&v7.branches) {
        approx(a.r, b.r, 1e-12, "v5/v7 branch r");
        approx(a.x, b.x, 1e-12, "v5/v7 branch x");
        approx(a.b, b.b, 1e-12, "v5/v7 branch b");
        approx(a.effective_tap(), b.effective_tap(), 1e-12, "v5/v7 tap");
    }
    for (a, b) in v5.generators.iter().zip(&v7.generators) {
        approx(a.pg, b.pg, 1e-12, "v5/v7 gen pg");
        approx(a.qg, b.qg, 1e-12, "v5/v7 gen qg");
    }
}

#[test]
fn v6_latin1_decodes_through_parse_file() {
    // The bus name carries a Latin-1 'o with diaeresis' (0xF6); the byte fast
    // path must decode it.
    let net = fixture("minimal_v6_latin1.dgs");
    assert_eq!(net.buses.len(), 2);
    assert!(
        net.buses[0].name.as_deref().unwrap().contains('\u{f6}'),
        "expected the decoded name, got {:?}",
        net.buses[0].name
    );
}

// ---- Warnings & ignored classes --------------------------------------------

#[test]
fn v7_warnings_itemize_ignored_and_fused() {
    let warnings = parse(&std::fs::read_to_string(data("dgs/minimal_v7.dgs")).unwrap()).warnings;
    let has = |sub: &str| warnings.iter().any(|w| w.contains(sub));
    assert!(has("fused 1 terminal pair"), "coupler fusion: {warnings:?}");
    assert!(
        has("`ElmFoobar` table ignored (1 rows)"),
        "unknown class: {warnings:?}"
    );
    assert!(
        has("`IntGrfcon` table ignored (1 rows)"),
        "graphics: {warnings:?}"
    );
    assert!(has("base_mva set to 100"), "base note: {warnings:?}");
}

// ---- Error cases -----------------------------------------------------------

#[test]
fn pfd_extension_is_rejected_with_guidance() {
    let err = parse_file(data("dgs/binary_reject.pfd"), None).unwrap_err();
    assert!(
        err.to_string().contains("encrypted PowerFactory"),
        "got: {err}"
    );
}

#[test]
fn opd_only_file_is_rejected() {
    let err = parse_file(data("dgs/opd_only_v7.dgs"), None).unwrap_err();
    assert!(
        err.to_string().contains("operational updates"),
        "got: {err}"
    );
}

const GENERAL_V7: &str = "$$General;FID(a:40);Descr(a:40);Val(a:40)\n1;Version;7.0\n";

fn with_general(body: &str) -> String {
    format!("{GENERAL_V7}{body}")
}

/// A minimal one-bus network body an edge case can extend.
fn one_bus() -> String {
    with_general(
        "$$ElmTerm;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);uknom(r)\n\
         B1;C;B1;Grid;110\n",
    )
}

#[test]
fn external_type_ref_on_a_line_is_fatal() {
    let doc = with_general(
        "$$ElmTerm;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);uknom(r)\n\
         B1;C;B1;Grid;110\nB2;C;B2;Grid;110\n\
         $$ElmLne;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);typ_id(p);dline(r)\n\
         Ln;C;Ln;Grid;##EXTERNAL;5\n\
         $$StaCubic;FID(a:40);OP(a:1);fold_id(p);obj_bus(i);obj_id(p)\n\
         C1;C;B1;0;Ln\nC2;C;B2;1;Ln\n",
    );
    let err = powerio::parse_str(&doc, "dgs").unwrap_err();
    assert!(
        err.to_string().contains("re-export from PowerFactory"),
        "got: {err}"
    );
}

#[test]
fn duplicate_table_name_is_an_error() {
    let doc = with_general(
        "$$ElmTerm;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);uknom(r)\n\
         B1;C;B1;Grid;110\n\
         $$ElmTerm;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);uknom(r)\n\
         B2;C;B2;Grid;110\n",
    );
    let err = powerio::parse_str(&doc, "dgs").unwrap_err();
    assert!(err.to_string().contains("duplicate table"), "got: {err}");
}

#[test]
fn duplicate_id_within_a_table_is_an_error() {
    let doc = with_general(
        "$$ElmTerm;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);uknom(r)\n\
         B1;C;B1;Grid;110\nB1;C;other;Grid;110\n",
    );
    let err = powerio::parse_str(&doc, "dgs").unwrap_err();
    assert!(err.to_string().contains("duplicate id"), "got: {err}");
}

#[test]
fn wrong_row_width_is_an_error() {
    let doc = with_general(
        "$$ElmTerm;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);uknom(r)\n\
         B1;C;B1;Grid\n", // one cell short
    );
    let err = powerio::parse_str(&doc, "dgs").unwrap_err();
    assert!(
        err.to_string().contains("cells, header declares"),
        "got: {err}"
    );
}

#[test]
fn missing_version_is_rejected() {
    let doc = "$$General;FID(a:40);Descr(a:40);Val(a:40)\n1;Author;me\n\
               $$ElmTerm;FID(a:40);loc_name(a:40);fold_id(p);uknom(r)\nB1;B1;Grid;110\n";
    let err = powerio::parse_str(doc, "dgs").unwrap_err();
    assert!(err.to_string().contains("no Version row"), "got: {err}");
}

#[test]
fn unsupported_version_is_rejected() {
    let doc = "$$General;FID(a:40);Descr(a:40);Val(a:40)\n1;Version;4.0\n\
               $$ElmTerm;FID(a:40);loc_name(a:40);fold_id(p);uknom(r)\nB1;B1;Grid;110\n";
    let err = powerio::parse_str(doc, "dgs").unwrap_err();
    assert!(err.to_string().contains("not supported"), "got: {err}");
}

// ---- Cell parsing edge cases -----------------------------------------------

#[test]
fn quoting_and_empty_cells() {
    // loc_name "B;1" carries the delimiter; typ_id is $empty$; chr_name empty.
    let doc = with_general(
        "$$ElmTerm;FID(a:40);OP(a:1);loc_name(a:40);typ_id(p);fold_id(p);chr_name(a:20);uknom(r)\n\
         B1;C;\"B;1\";$empty$;Grid;;110\n",
    );
    let net = powerio::parse_str(&doc, "dgs").unwrap().network;
    assert_eq!(net.buses[0].name.as_deref(), Some("B;1"));
    // $empty$ and empty cells are absent from extras (dgs_* only stores values).
    assert!(!net.buses[0].extras.contains_key("dgs_typ_id"));
    assert!(!net.buses[0].extras.contains_key("dgs_chr_name"));
}

#[test]
fn decimal_separator_comma_is_honored() {
    let doc = "$$General;FID(a:40);Descr(a:40);Val(a:40)\n\
               1;Version;7.0\n2;DecimalSeparator;,\n\
               $$ElmTerm;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);uknom(r)\n\
               B1;C;B1;Grid;20,5\n";
    let net = powerio::parse_str(doc, "dgs").unwrap().network;
    approx(net.buses[0].base_kv, 20.5, 1e-9, "comma decimal");
}

#[test]
fn op_flags_control_row_inclusion() {
    // D=delete (skip), I=ignore (skip silent), U=update (kept + warning),
    // C/M/empty=create.
    let doc = with_general(
        "$$ElmTerm;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);uknom(r)\n\
         B1;C;B1;Grid;110\n\
         B2;D;B2;Grid;110\n\
         B3;I;B3;Grid;110\n\
         B4;U;B4;Grid;110\n\
         B5;M;B5;Grid;110\n",
    );
    let parsed = powerio::parse_str(&doc, "dgs").unwrap();
    // B1 (C), B4 (U), B5 (M) kept; B2 (D) and B3 (I) dropped.
    assert_eq!(parsed.network.buses.len(), 3);
    assert!(parsed.warnings.iter().any(|w| w.contains("update row")));
    assert!(parsed.warnings.iter().any(|w| w.contains("delete row")));
}

#[test]
fn hash_id_rows_are_skipped() {
    // A `##`-keyed row references an external project; only B1 survives.
    let doc = with_general(
        "$$ElmTerm;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);uknom(r)\n\
         B1;C;B1;Grid;110\n##EXT;M;ext;Grid;110\n",
    );
    let parsed = powerio::parse_str(&doc, "dgs").unwrap();
    assert_eq!(parsed.network.buses.len(), 1);
    assert!(
        parsed
            .warnings
            .iter()
            .any(|w| w.contains("external PowerFactory"))
    );
}

#[test]
fn coupler_fusion_and_open_switch() {
    // T1-T2 closed coupler => fused; T2-T3 open coupler => switch.
    let doc = with_general(
        "$$ElmTerm;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);uknom(r)\n\
         T1;C;T1;Grid;20\nT2;C;T2;Grid;20\nT3;C;T3;Grid;20\n\
         $$ElmCoup;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);on_off(i)\n\
         Cc;C;Cc;Grid;1\nCo;C;Co;Grid;0\n\
         $$StaCubic;FID(a:40);OP(a:1);fold_id(p);obj_bus(i);obj_id(p)\n\
         K1;C;T1;0;Cc\nK2;C;T2;1;Cc\nK3;C;T2;0;Co\nK4;C;T3;1;Co\n",
    );
    let net = powerio::parse_str(&doc, "dgs").unwrap().network;
    // T1+T2 fused => 2 buses; the open coupler is a switch between them.
    assert_eq!(net.buses.len(), 2);
    assert_eq!(net.switches.len(), 1);
    assert!(!net.switches[0].closed);
}

#[test]
fn reactor_shunt_sign_is_negative() {
    // shtype 1 (reactor) => negative susceptance.
    let doc = with_general(
        "$$ElmTerm;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);uknom(r)\nB1;C;B1;Grid;20\n\
         $$ElmShnt;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);shtype(i);ncapa(i);qtotn(r);ushnm(r)\n\
         S1;C;S1;Grid;1;1;3;20\n\
         $$StaCubic;FID(a:40);OP(a:1);fold_id(p);obj_bus(i);obj_id(p)\nK1;C;B1;0;S1\n",
    );
    let net = powerio::parse_str(&doc, "dgs").unwrap().network;
    approx(net.shunts[0].b, -3.0, 1e-9, "reactor b<0");
}

#[test]
fn unconnected_element_is_skipped_with_warning() {
    // A load with no cubicle is dropped, not fatal.
    let doc = format!(
        "{}$$ElmLod;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);plini(r);qlini(r)\n\
         Ld;C;Ld;Grid;5;2\n",
        one_bus()
    );
    let parsed = powerio::parse_str(&doc, "dgs").unwrap();
    assert!(parsed.network.loads.is_empty());
    assert!(
        parsed
            .warnings
            .iter()
            .any(|w| w.contains("no connected cubicle"))
    );
}

// ---- Writer ----------------------------------------------------------------

#[test]
fn writer_golden_output() {
    let mut net = Network::in_memory("Tiny", 100.0, vec![], vec![]);
    net.base_frequency = 50.0;
    let mut b1 = Bus::new(BusId(1), BusType::Ref, 110.0);
    b1.name = Some("A".into());
    let mut b2 = Bus::new(BusId(2), BusType::Pq, 110.0);
    b2.name = Some("B".into());
    net.buses = vec![b1, b2];
    let mut br = Branch::new(BusId(1), BusId(2), 0.01, 0.1);
    br.b = 0.02;
    br.rate_a = 100.0;
    net.branches = vec![br];
    let mut g = Generator::new(BusId(1));
    g.pg = 50.0;
    g.qg = 10.0;
    g.vg = 1.0;
    g.mbase = 100.0;
    g.pmax = 100.0;
    g.qmax = 50.0;
    g.qmin = -50.0;
    net.generators = vec![g];
    net.loads = vec![Load::new(BusId(2), 30.0, 15.0)];

    let expected = concat!(
        "********************************************************************************\r\n",
        "*\r\n* powerio DGS export\r\n*\r\n",
        "********************************************************************************\r\n\r\n",
        "$$General;FID(a:40);Descr(a:40);Val(a:40)\r\n1;Version;7.0\r\n\r\n",
        "$$ElmNet;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);frnom(r)\r\nGrid;C;Tiny;;50\r\n\r\n",
        "$$ElmTerm;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);iUsage(i);outserv(i);uknom(r)\r\n",
        "TERM1;C;A;Grid;0;0;110\r\nTERM2;C;B;Grid;0;0;110\r\n\r\n",
        "$$ElmXnet;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);bustp(a:2);outserv(i);pgini(r);qgini(r);usetp(r);snss(r)\r\n",
        "XNET3;C;XNET3;Grid;SL;0;50;10;1;10000\r\n\r\n",
        "$$ElmLod;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);outserv(i);plini(r);qlini(r);scale0(r)\r\n",
        "LOD4;C;LOD4;Grid;0;30;15;1\r\n\r\n",
        "$$ElmLne;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);typ_id(p);dline(r);fline(r);outserv(i)\r\n",
        "LNE5;C;LNE5;Grid;TLNE5;1;1;0\r\n\r\n",
        "$$TypLne;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);rline(r);xline(r);cline(r);uline(r);sline(r);nlnph(i)\r\n",
        "TLNE5;C;TLNE5;Grid;1.21;12.100000000000001;0.5261320432789929;110;0.524863881081478;3\r\n\r\n",
        "$$StaCubic;FID(a:40);OP(a:1);loc_name(a:40);fold_id(p);obj_bus(i);obj_id(p)\r\n",
        "CUB1;C;CUB1;TERM1;0;XNET3\r\nCUB2;C;CUB2;TERM2;0;LOD4\r\n",
        "CUB3;C;CUB3;TERM1;0;LNE5\r\nCUB4;C;CUB4;TERM2;1;LNE5\r\n\r\n",
    );
    assert_eq!(write_dgs(&net).text, expected);
}

#[test]
fn writer_reader_numeric_round_trip() {
    // A DGS-sourced network re-exports and re-reads to the same values.
    let net0 = fixture("minimal_v7.dgs");
    let net1 = parse_dgs(&write_dgs(&net0).text).unwrap();
    assert_eq!(net0.buses.len(), net1.buses.len());
    assert_eq!(net0.branches.len(), net1.branches.len());
    for (a, b) in net0.branches.iter().zip(&net1.branches) {
        approx(a.r, b.r, 1e-9, "rt branch r");
        approx(a.x, b.x, 1e-9, "rt branch x");
        approx(a.effective_tap(), b.effective_tap(), 1e-9, "rt tap");
    }
    for (a, b) in net0.generators.iter().zip(&net1.generators) {
        approx(a.pg, b.pg, 1e-6, "rt gen pg");
        approx(a.vg, b.vg, 1e-9, "rt gen vg");
    }
}

#[test]
fn writer_warns_on_unrepresentable_data() {
    let mut net = Network::in_memory("W", 100.0, vec![], vec![]);
    net.buses = vec![Bus::new(BusId(1), BusType::Ref, 100.0)];
    let mut g = Generator::new(BusId(1));
    g.cost = Some(powerio::GenCost::new(2, 0.0, 0.0, vec![0.01, 2.0, 0.0]));
    net.generators = vec![g];
    net.storage = vec![powerio::Storage::new(BusId(1))];
    let conv = write_dgs(&net);
    assert!(conv.warnings.iter().any(|w| w.contains("cost")));
    assert!(conv.warnings.iter().any(|w| w.contains("storage")));
}

// ---- MATPOWER oracle -------------------------------------------------------

fn by_bus<F: Fn(&Network) -> Vec<(usize, f64, f64)>>(
    net: &Network,
    f: F,
) -> BTreeMap<usize, (f64, f64)> {
    let mut m = BTreeMap::new();
    for (bus, a, b) in f(net) {
        let e = m.entry(bus).or_insert((0.0, 0.0));
        e.0 += a;
        e.1 += b;
    }
    m
}

#[test]
fn matpower_oracle_round_trip() {
    for case in ["case9.m", "case14.m", "case30.m", "case57.m", "case118.m"] {
        let net = parse_matpower_file(data(case)).unwrap();
        let back = parse_dgs(&write_dgs(&net).text).unwrap();

        assert_eq!(net.buses.len(), back.buses.len(), "{case} bus count");
        assert_eq!(
            net.branches.len(),
            back.branches.len(),
            "{case} branch count"
        );
        assert_eq!(
            net.generators.len(),
            back.generators.len(),
            "{case} gen count"
        );

        // Per-bus kv.
        let kv0: BTreeMap<BusId, f64> = net.buses.iter().map(|b| (b.id, b.base_kv)).collect();
        for b in &back.buses {
            approx(
                kv0[&b.id],
                b.base_kv,
                1e-9,
                &format!("{case} bus {} kv", b.id),
            );
        }
        // Per-bus load & shunt totals.
        let l0 = by_bus(&net, |n| {
            n.loads.iter().map(|l| (l.bus.0, l.p, l.q)).collect()
        });
        let l1 = by_bus(&back, |n| {
            n.loads.iter().map(|l| (l.bus.0, l.p, l.q)).collect()
        });
        assert_eq!(l0, l1, "{case} load totals");
        let s0 = by_bus(&net, |n| {
            n.shunts.iter().map(|s| (s.bus.0, s.g, s.b)).collect()
        });
        let s1 = by_bus(&back, |n| {
            n.shunts.iter().map(|s| (s.bus.0, s.g, s.b)).collect()
        });
        assert_eq!(s0.len(), s1.len(), "{case} shunt bus count");
        for (k, v0) in &s0 {
            approx(v0.0, s1[k].0, 1e-9, &format!("{case} shunt g {k}"));
            approx(v0.1, s1[k].1, 1e-9, &format!("{case} shunt b {k}"));
        }
        // Gen dispatch.
        let g0 = by_bus(&net, |n| {
            n.generators.iter().map(|g| (g.bus.0, g.pg, g.qg)).collect()
        });
        let g1 = by_bus(&back, |n| {
            n.generators.iter().map(|g| (g.bus.0, g.pg, g.qg)).collect()
        });
        for (k, v0) in &g0 {
            approx(v0.0, g1[k].0, 1e-6, &format!("{case} gen pg {k}"));
            approx(v0.1, g1[k].1, 1e-6, &format!("{case} gen qg {k}"));
        }
        // Branch r/x/tap/shift/rate_a (b excluded for transformers).
        let bp = |n: &Network| -> BTreeMap<(usize, usize), BranchTuple> {
            n.branches
                .iter()
                .map(|b| {
                    (
                        (b.from.0, b.to.0),
                        (
                            b.r,
                            b.x,
                            b.legacy_total_charging_b(),
                            b.effective_tap(),
                            b.shift,
                            b.rate_a,
                            b.is_transformer(),
                        ),
                    )
                })
                .collect()
        };
        let (m0, m1) = (bp(&net), bp(&back));
        for (k, v0) in &m0 {
            let v1 = m1[k];
            approx(v0.0, v1.0, 1e-9, &format!("{case} branch r {k:?}"));
            approx(v0.1, v1.1, 1e-9, &format!("{case} branch x {k:?}"));
            if !v0.6 {
                approx(v0.2, v1.2, 1e-9, &format!("{case} branch b {k:?}"));
            }
            approx(v0.3, v1.3, 1e-9, &format!("{case} branch tap {k:?}"));
            approx(v0.4, v1.4, 1e-9, &format!("{case} branch shift {k:?}"));
            approx(v0.5, v1.5, 1e-6, &format!("{case} branch rate_a {k:?}"));
        }
    }
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
    let path = Path::new(&reference);
    if !path.exists() {
        eprintln!("reference file absent; skipping");
        return;
    }
    let parsed = parse_file(path, None).unwrap();
    assert!(
        !parsed.network.buses.is_empty(),
        "reference parsed with buses"
    );
    eprintln!(
        "{reference}: {} buses, {} branches, {} gens",
        parsed.network.buses.len(),
        parsed.network.branches.len(),
        parsed.network.generators.len()
    );
}
