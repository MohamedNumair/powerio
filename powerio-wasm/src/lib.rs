//! WebAssembly surface for powerio — the web `WebEngine` backend.
//!
//! M0 stub: a single no-op [`version`] export that compiles to
//! `wasm32-unknown-unknown`. The real `convert` / `parse_summary` /
//! `extract_tables` exports (mirroring the desktop C ABI) land in M2.

use wasm_bindgen::prelude::*;

/// Returns this crate's version string — a build/ABI sanity check callable
/// from JS before any real work.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
