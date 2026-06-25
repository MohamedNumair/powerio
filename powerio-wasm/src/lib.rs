//! WebAssembly surface for powerio — the web `WebEngine` backend.
//!
//! A thin `wasm-bindgen` wrapper over `powerio` (transmission) and
//! `powerio-dist` (distribution) that mirrors the desktop C ABI
//! (`powerio-capi`, `PIO_ABI_VERSION = 4`) so the Flutter `WebEngine` drives the
//! same convert / summary / matrix UI **100% client-side** (plan §2, §4 "Web").
//!
//! Every export returns plain-data JS objects that map 1:1 to the Dart DTOs the
//! desktop FFI backend produces (`ConvertResult`, the M3 `CaseSummary`, the M4
//! `NetworkTables`), so the `PowerioEngine` abstraction stays platform-agnostic.
//! Parity is the contract: each function calls the **same** upstream Rust the C
//! ABI calls — `powerio::convert_str` / `powerio_dist::convert_str` behind
//! `pio_convert_str` / `pio_dist_convert_str`, `IndexCore::build` +
//! `IndexedNetwork::with_core` behind the per-bus extractors, and
//! `Network::to_normalized` behind `pio_normalize` — so `DesktopEngine` (FFI)
//! and `WebEngine` (WASM) agree byte-for-byte (plan §8 cross-backend parity,
//! §12 risk 2).
//!
//! The four exports and their JS payload shapes:
//! - `version() -> string`
//! - `convert(text, from, to) -> { text: string, warnings: string[] }`
//! - `parse_summary(text, from) -> { buses, branches, gens, baseMva, islands,
//!   radial, warnings }`
//! - `extract_tables(text, from, normalized) -> { busIds, branches:{from,to,r,x,
//!   b,tap,shift,inService}, demand:{pd,qd}, shunt:{gs,bs} }`
//!
//! Fallible exports return `Result<JsValue, JsValue>`; the `Err` carries the
//! upstream error message as a string — no panics or traps cross the boundary.

use powerio::{IndexCore, IndexedNetwork, TargetFormat};
use powerio_dist::{DistTargetFormat, dist_target_from_name};
use serde::Serialize;
use wasm_bindgen::prelude::*;

/// The crate version string — the web mirror of the C ABI's informational
/// `pio_version()` (both resolve to the workspace version). The `WebEngine`
/// cross-checks this at startup as the wasm analogue of the desktop
/// `pio_abi_version()` handshake (plan §8 item 4).
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

// ── convert ─────────────────────────────────────────────────────────────────

/// `{ text, warnings }` — the web mirror of the desktop `ConvertResult` DTO.
/// Warnings are read-side first (the upstream `convert_str` ordering) and are
/// never dropped — the conversion-notes honesty contract (plan §5.1).
#[derive(Debug, Serialize)]
struct ConvertOut {
    text: String,
    warnings: Vec<String>,
}

/// Convert `text` from format `from` to format `to`, returning
/// `{ text, warnings[] }`.
///
/// Dispatches transmission vs. distribution exactly as `DesktopEngine` does: the
/// distribution domain is the OpenDSS / PMD / BMOPF family (recognized by
/// `dist_target_from_name`); the two domains do not interconvert, so a mixed
/// pair is rejected up front with the same message the desktop backend uses.
/// Transmission rides `powerio::convert_str`, distribution
/// `powerio_dist::convert_str` — the calls behind `pio_convert_str` /
/// `pio_dist_convert_str`. `from` is forwarded as a raw name string (so PSLF
/// source aliases `epc`/`pslf` work); only `to` is parsed to the target enum,
/// mirroring the C ABI's `to.parse::<TargetFormat>()`.
#[wasm_bindgen]
pub fn convert(text: &str, from: &str, to: &str) -> Result<JsValue, JsValue> {
    to_js(&convert_inner(text, from, to).map_err(|e| err_to_js(&e))?)
}

fn convert_inner(text: &str, from: &str, to: &str) -> Result<ConvertOut, String> {
    let from_dist = dist_target_from_name(from).is_some();
    let to_dist = dist_target_from_name(to).is_some();
    if from_dist != to_dist {
        return Err(format!(
            "Cannot convert between transmission and distribution formats \
             ({from} → {to}) — they are separate domains."
        ));
    }
    let (text, warnings) = if from_dist {
        let target = to.parse::<DistTargetFormat>().map_err(|e| e.to_string())?;
        let conv = powerio_dist::convert_str(text, target, from).map_err(|e| e.to_string())?;
        (conv.text, conv.warnings)
    } else {
        let target = to.parse::<TargetFormat>().map_err(|e| e.to_string())?;
        let conv = powerio::convert_str(text, target, from).map_err(|e| e.to_string())?;
        (conv.text, conv.warnings)
    };
    Ok(ConvertOut { text, warnings })
}

// ── parse_summary ─────────────────────────────────────────────────────────────

/// Case summary — the web mirror of the M3 `CaseSummary` DTO. Counts are the
/// raw (unexpanded) `Network` table lengths (as `pio_n_*`); `islands`/`radial`
/// come from the indexed in-service topology (as `pio_n_islands`/
/// `pio_is_radial`); `warnings` are the reader's fidelity notes (as
/// `pio_warnings`). Transmission-only, matching the C ABI's bus-granular surface
/// (the `pio_n_*` family takes the transmission `PioNetwork`, not the dist
/// handle).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SummaryOut {
    buses: usize,
    branches: usize,
    gens: usize,
    base_mva: f64,
    islands: usize,
    radial: bool,
    warnings: Vec<String>,
}

/// Parse `text` of format `from` and return its case summary (plan §5.2). The
/// mirror of `pio_parse_str` followed by `pio_n_buses`/`pio_n_branches`/
/// `pio_n_gens`/`pio_base_mva`/`pio_n_islands`/`pio_is_radial`/`pio_warnings`.
#[wasm_bindgen]
pub fn parse_summary(text: &str, from: &str) -> Result<JsValue, JsValue> {
    to_js(&parse_summary_inner(text, from).map_err(|e| err_to_js(&e))?)
}

fn parse_summary_inner(text: &str, from: &str) -> Result<SummaryOut, String> {
    let parsed = powerio::parse_str(text, from).map_err(|e| e.to_string())?;
    let net = parsed.network;
    // Build the IndexCore once and view through it, exactly as the C ABI handle
    // does (make_network → IndexCore::build; view → IndexedNetwork::with_core).
    let core = IndexCore::build(&net);
    let view = IndexedNetwork::with_core(&net, &core);
    Ok(SummaryOut {
        buses: net.buses.len(),
        branches: net.branches.len(),
        gens: net.generators.len(),
        base_mva: net.base_mva,
        islands: view.n_connected_components(),
        radial: view.is_radial(),
        warnings: parsed.warnings,
    })
}

// ── extract_tables ────────────────────────────────────────────────────────────

/// The branch table as parallel columns, mirroring `pio_branches` exactly:
/// `from`/`to` are 1-based **bus IDs** (the `busIds` id space, not dense
/// indices), in branch source order; `r`/`x`/`b`/`tap`/`shift` are emitted
/// verbatim (no 0→1 tap mapping or degree→radian conversion here — those land
/// only via `to_normalized`); `inService` is 0/1.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BranchTable {
    from: Vec<i64>,
    to: Vec<i64>,
    r: Vec<f64>,
    x: Vec<f64>,
    b: Vec<f64>,
    tap: Vec<f64>,
    shift: Vec<f64>,
    in_service: Vec<u8>,
}

/// Per-bus demand aggregates (`pio_bus_demand`): active `pd`, reactive `qd`,
/// summed over each bus's loads in dense `busIds` order.
#[derive(Debug, Serialize)]
struct Demand {
    pd: Vec<f64>,
    qd: Vec<f64>,
}

/// Per-bus shunt aggregates (`pio_bus_shunt`): conductance `gs`, susceptance
/// `bs`, in dense `busIds` order.
#[derive(Debug, Serialize)]
struct Shunt {
    gs: Vec<f64>,
    bs: Vec<f64>,
}

/// The numeric tables the Dart matrix assembler consumes (plan §5.3) — the web
/// mirror of the M4 `NetworkTables` DTO and of the C ABI extractors
/// `pio_bus_ids`/`pio_branches`/`pio_bus_demand`/`pio_bus_shunt`. `busIds`
/// defines the dense index space the per-bus columns share.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TablesOut {
    bus_ids: Vec<i64>,
    branches: BranchTable,
    demand: Demand,
    shunt: Shunt,
}

/// Extract the numeric tables for matrix assembly (plan §5.3). When
/// `normalized == true`, operate on the normalized view (per unit, radians,
/// out-of-service filtered, tap 0→1) — the wasm equivalent of `pio_normalize`
/// followed by the extractors, so the per-unit scaling lands in the network
/// fields before the `IndexCore` sums them, identical to the desktop path.
#[wasm_bindgen]
pub fn extract_tables(text: &str, from: &str, normalized: bool) -> Result<JsValue, JsValue> {
    to_js(&extract_tables_inner(text, from, normalized).map_err(|e| err_to_js(&e))?)
}

fn extract_tables_inner(text: &str, from: &str, normalized: bool) -> Result<TablesOut, String> {
    let parsed = powerio::parse_str(text, from).map_err(|e| e.to_string())?;
    let net = if normalized {
        parsed.network.to_normalized().map_err(|e| e.to_string())?
    } else {
        parsed.network
    };
    let core = IndexCore::build(&net);
    let view = IndexedNetwork::with_core(&net, &core);

    let bus_ids: Vec<i64> = net
        .buses
        .iter()
        .map(|b| i64::try_from(b.id.0).unwrap_or(-1))
        .collect();
    let branches = BranchTable {
        from: net
            .branches
            .iter()
            .map(|br| i64::try_from(br.from.0).unwrap_or(-1))
            .collect(),
        to: net
            .branches
            .iter()
            .map(|br| i64::try_from(br.to.0).unwrap_or(-1))
            .collect(),
        r: net.branches.iter().map(|br| br.r).collect(),
        x: net.branches.iter().map(|br| br.x).collect(),
        b: net.branches.iter().map(|br| br.b).collect(),
        tap: net.branches.iter().map(|br| br.tap).collect(),
        shift: net.branches.iter().map(|br| br.shift).collect(),
        in_service: net
            .branches
            .iter()
            .map(|br| u8::from(br.in_service))
            .collect(),
    };
    let demand = Demand {
        pd: view.pd().to_vec(),
        qd: view.qd().to_vec(),
    };
    let shunt = Shunt {
        gs: view.gs().to_vec(),
        bs: view.bs().to_vec(),
    };
    Ok(TablesOut {
        bus_ids,
        branches,
        demand,
        shunt,
    })
}

// ── marshalling helpers ───────────────────────────────────────────────────────

/// Serialize a DTO to a plain JS object (not a `Map`), or surface the
/// serialization failure as a `JsValue` string.
fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(value).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Map an upstream error message to a thrown JS value (a string), matching how
/// `DesktopEngine` throws on a NULL/`errbuf` return so the UI error path is
/// identical across backends.
fn err_to_js(message: &str) -> JsValue {
    JsValue::from_str(message)
}

// The `#[wasm_bindgen]` exports return `JsValue`, which needs a JS runtime, so
// these host tests exercise the pure-Rust `*_inner` cores (the rlib crate-type
// keeps the crate host-testable: `cargo test -p powerio-wasm`). They mirror the
// desktop M1 smoke (`app/test/engine/desktop_convert_test.dart`) on the same
// case14.m / OpenDSS fixtures, so a divergence here is the same divergence the
// FFI==WASM parity gate (#30) would catch. Byte-exact goldens land in #30.
#[cfg(test)]
mod tests {
    use super::*;

    const CASE14_M: &str = include_str!("../tests/fixtures/case14.m");
    const LINECODE_DSS: &str = include_str!("../tests/fixtures/linecode_10x10.dss");

    #[test]
    fn version_is_the_workspace_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn convert_matpower_to_psse_produces_non_matpower_text() {
        let out = convert_inner(CASE14_M, "matpower", "psse").expect("convert should succeed");
        // Mirrors the desktop smoke: non-trivial output that is no longer
        // MATPOWER source. Warnings are carried (read-side first); exact
        // contents are byte-gated in #30.
        assert!(out.text.trim().len() > 200, "expected non-trivial PSS/E text");
        assert!(!out.text.contains("mpc.bus"), "output should not be MATPOWER");
    }

    #[test]
    fn convert_rejects_cross_domain_like_desktop() {
        let err = convert_inner(CASE14_M, "matpower", "dss").unwrap_err();
        assert!(err.contains("separate domains"), "got: {err}");
    }

    #[test]
    fn convert_invalid_input_is_err_not_panic() {
        assert!(convert_inner(CASE14_M, "matpower", "not-a-format").is_err());
        assert!(convert_inner("not a case file", "matpower", "psse").is_err());
    }

    #[test]
    fn summary_matches_ieee_case14() {
        let s = parse_summary_inner(CASE14_M, "matpower").expect("summary should succeed");
        assert_eq!(s.buses, 14);
        assert_eq!(s.branches, 20);
        assert_eq!(s.gens, 5);
        assert!((s.base_mva - 100.0).abs() < 1e-9);
        assert_eq!(s.islands, 1);
        assert!(!s.radial, "IEEE case14 is meshed, not radial");
    }

    #[test]
    fn extract_tables_shapes_match_case14() {
        let raw = extract_tables_inner(CASE14_M, "matpower", false).expect("raw tables");
        assert_eq!(raw.bus_ids.len(), 14);
        assert_eq!(raw.branches.from.len(), 20);
        assert_eq!(raw.branches.to.len(), 20);
        assert_eq!(raw.branches.r.len(), 20);
        assert_eq!(raw.branches.in_service.len(), 20);
        // Per-bus columns share the busIds dense order, so all are length n.
        assert_eq!(raw.demand.pd.len(), 14);
        assert_eq!(raw.demand.qd.len(), 14);
        assert_eq!(raw.shunt.gs.len(), 14);
        assert_eq!(raw.shunt.bs.len(), 14);

        // The normalized path (per unit / radians / tap 0->1 / filtered) must
        // run without error; case14 is fully in service so counts are unchanged.
        let norm = extract_tables_inner(CASE14_M, "matpower", true).expect("normalized tables");
        assert_eq!(norm.bus_ids.len(), 14);
        assert_eq!(norm.branches.from.len(), 20);
    }

    #[test]
    fn dist_convert_dss_to_pmd_is_json() {
        // OpenDSS -> PMD ENGINEERING JSON through the same convert() entry (dist
        // branch), mirroring the desktop dist smoke.
        let out = convert_inner(LINECODE_DSS, "dss", "pmd").expect("dss->pmd should succeed");
        assert!(out.text.trim_start().starts_with('{'), "PMD ENGINEERING is JSON");
        assert!(out.text.len() > 50);
    }
}
