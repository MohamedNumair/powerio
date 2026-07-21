//! Read DIgSILENT PowerFactory DGS ASCII cases into the multiconductor
//! [`DistNetwork`](crate::DistNetwork).
//!
//! This is the distribution counterpart to the balanced DGS reader in the
//! `powerio` crate. The two are independent: the balanced reader collapses DGS
//! to a positive-sequence [`powerio::Network`], while this one keeps the
//! zero-sequence and per-phase data and reconstructs a genuinely unbalanced
//! three-phase network — Fortescue 3x3 line matrices (or an explicit natural
//! coordinate matrix when DGS exports one), wye/delta transformers from the
//! vector group, per-phase loads, and one `VoltageSource` from the external
//! grid. The whole module is self-contained: powerio-dist keeps its zero
//! internal-crate dependencies, so the DGS scanner is re-implemented here
//! rather than shared with `powerio`.
//!
//! DGS is a read-only distribution source. There is no DGS writer on the
//! distribution model, so a parsed network leaves `source_format` unset and is
//! never treated as an echoable write target: `powerio convert feeder.dgs --to
//! dss|pmd|bmopf` regenerates from the typed model.
//!
//! The layers mirror the balanced reader: layer 1 scans the class tables
//! ([`scan`]), layer 2 resolves the cubicle/coupler topology ([`object`]), and
//! layer 3 maps to the multiconductor model ([`map`]); [`matrix`] is the
//! matrix-valued attribute plumbing the line ladder needs.

mod map;
mod matrix;
mod object;
mod scan;

use std::path::Path;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::model::DistNetwork;

/// The stable format label carried on every DGS read error and warning.
pub(crate) const FMT: &str = "DIgSILENT DGS";

/// Parse decoded DGS text into a [`DistNetwork`].
///
/// Read warnings accumulate on [`DistNetwork::warnings`]; every fidelity loss
/// is itemized there. Malformed input returns [`Error::FormatRead`] rather than
/// panicking.
pub fn parse_dgs_str(text: &str) -> Result<DistNetwork> {
    sniff_binary(text)?;
    let mut warnings = Vec::new();
    let doc = scan::scan(text, &mut warnings)?;
    let mut net = map::Reader::build(&doc, None)?;
    // The scanner's own warnings (encoding, operation flags) precede the
    // mapper's, matching the balanced reader's ordering.
    let mut all = warnings;
    all.append(&mut net.warnings);
    net.warnings = all;
    // Retain the decoded text for provenance only; with `source_format` unset
    // it never rides the byte-exact echo tier.
    net.source = Some(Arc::new(text.to_owned()));
    Ok(net)
}

/// Parse a DGS file into a [`DistNetwork`], decoding the bytes first.
///
/// The official example files ship as ISO-8859-1 (Latin-1) with CRLF; some
/// tools emit UTF-8 or UTF-16. The BOM is sniffed, then strict UTF-8, then a
/// Latin-1 fallback that cannot fail.
pub fn parse_dgs_file(path: impl AsRef<Path>) -> Result<DistNetwork> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|source| Error::Io {
        path: path.display().to_string(),
        source,
    })?;
    let text = decode_dgs_bytes(&bytes);
    let hint = path.file_stem().and_then(|s| s.to_str());
    sniff_binary(&text)?;
    let mut warnings = Vec::new();
    let doc = scan::scan(&text, &mut warnings)?;
    let mut net = map::Reader::build(&doc, hint)?;
    let mut all = warnings;
    all.append(&mut net.warnings);
    net.warnings = all;
    net.source = Some(Arc::new(text));
    Ok(net)
}

/// Decode raw DGS bytes into text: sniff the BOM, then strict UTF-8, then a
/// Latin-1 fallback where every byte maps to a code point.
fn decode_dgs_bytes(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, false);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, true);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        // Latin-1: each byte is its own Unicode scalar value (0..=255).
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// Decode UTF-16 code units after the BOM has been stripped. Lone surrogates
/// and a trailing odd byte become the replacement character.
fn decode_utf16(bytes: &[u8], big_endian: bool) -> String {
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|pair| {
            let hi = pair[0];
            let lo = *pair.get(1).unwrap_or(&0);
            if big_endian {
                u16::from_be_bytes([hi, lo])
            } else {
                u16::from_le_bytes([hi, lo])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// The friendly rejection for an encrypted `.pfd` project export (or any binary
/// payload): tell the caller how to produce an ASCII DGS instead.
fn pfd_rejection() -> Error {
    Error::FormatRead {
        format: FMT,
        message: "this looks like an encrypted PowerFactory export (.pfd or binary). \
                  Export ASCII DGS instead: PowerFactory \u{2192} File \u{2192} Export \u{2192} \
                  DGS (*.dgs), DGS version 6.0 or 7.0"
            .into(),
    }
}

/// Reject binary/encrypted content up front: a control byte other than
/// tab/CR/LF, or a document with no `$$` table header at all, is not ASCII DGS.
fn sniff_binary(source: &str) -> Result<()> {
    if source
        .bytes()
        .any(|b| b < 0x09 || (0x0e..0x20).contains(&b) || b == 0x0b || b == 0x0c)
    {
        return Err(pfd_rejection());
    }
    if !source.lines().any(|l| l.trim_start().starts_with("$$")) {
        return Err(pfd_rejection());
    }
    Ok(())
}
