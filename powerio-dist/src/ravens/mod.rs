//! MG-RAVENS JSON import and export for the multiconductor
//! [`DistNetwork`](crate::model::DistNetwork).
//!
//! MG-RAVENS (<https://github.com/lanl-ansi/MG-RAVENS>, Apache-2.0) is the
//! LANL/Triad microgrid data schema: one JSON document of name-keyed hash
//! tables nested under the CIM class hierarchy, with `Ravens.cimObjectType`
//! naming each object's concrete class and `Class::'name'` strings pointing
//! between tables. Values are SI (volts, watts, ohms, meters) and radians.
//!
//! This is the multiconductor sibling of the balanced MG-RAVENS support in
//! the `powerio` crate: that reader takes the MATPOWER-derived transmission
//! profile and refuses documents with per-phase detail; this one takes the
//! OpenDSS-derived distribution profile — `ACLineSegmentPhase`,
//! `EnergyConsumerPhase`, `PerLengthPhaseImpedance` matrices,
//! `TransformerTank` ends with catalog `TransformerEndInfo` records,
//! `EnergySource` Thevenin sources — and refuses the balanced profile's
//! markers, pointing back. Because it lands in `DistNetwork`, a RAVENS
//! feeder converts to OpenDSS `.dss`, PowerModelsDistribution ENGINEERING
//! JSON, and BMOPF JSON through the same
//! [`DistTargetFormat`](crate::DistTargetFormat) machinery as every other
//! distribution source, and any of those convert into RAVENS.
//!
//! The writer ([`write_ravens_json`]) emits deterministic UUID-shaped mRIDs
//! derived from class and name (imported mRIDs pass through the
//! `ravens_mrid` extras), so write → read → write is byte stable; parses
//! retain their source for the byte-exact echo tier.
//!
//! # Phase convention
//!
//! CIM `SinglePhaseKind`/`PhaseCode` letters map to the wire-coordinate
//! terminal names (OpenDSS node numbers) the rest of the crate uses:
//! `A`→`1`, `B`→`2`, `C`→`3`, `N`→`4`, and the split-phase secondary codes
//! `s1`/`s2`→`1`/`2`.

mod read;
mod write;

pub use read::{parse_ravens_file, parse_ravens_str};
pub use write::write_ravens_json;

/// Extras key carrying an imported `IdentifiedObject.mRID`. Prefixed like
/// `pmd_*` so the other writers treat it as converter bookkeeping, not data.
pub(crate) const MRID_KEY: &str = "ravens_mrid";

/// Extras key carrying `Switch.normalOpen` when it differs from the live
/// state (the typed model only has `open`).
pub(crate) const NORMAL_OPEN_KEY: &str = "ravens_normal_open";

/// FNV-1a 64 over `bytes`, from `seed`.
fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = seed;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// A UUID-shaped identifier derived from the object's class and name, the
/// same derivation as the balanced writer in the `powerio` crate. The
/// upstream converters write a fresh random UUID4 per run; a name-derived id
/// keeps canonical output idempotent (write → parse → write is byte stable)
/// and diffs meaningful. The schema types `mRID` as a plain string with
/// UUIDs recommended, so a deterministic UUID-shaped value is valid. When
/// the element carries a round-tripped source mRID in its extras, the
/// caller passes that through instead.
pub(crate) fn det_mrid(kind: &str, name: &str) -> String {
    let tag = format!("powerio-ravens:{kind}:{name}");
    let hi = fnv1a(0xcbf2_9ce4_8422_2325, tag.as_bytes());
    let lo = fnv1a(0x6c62_272e_07bb_0142, tag.as_bytes());
    // Stamp the version (4) and variant (10xx) nibbles so the id parses as a
    // syntactically valid RFC 4122 UUID.
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

/// Split a trailing digit run so hash keys order numerically (`line10` after
/// `line2`); the upstream converters number elements, and hash tables carry
/// no order of their own.
pub(crate) fn natural_key(name: &str) -> (String, u64, String) {
    let stem_len = name.len() - name.bytes().rev().take_while(u8::is_ascii_digit).count();
    let (stem, digits) = name.split_at(stem_len);
    (
        stem.to_owned(),
        digits.parse().unwrap_or(0),
        name.to_owned(),
    )
}

/// The terminal name (OpenDSS node number) for a CIM phase code letter.
pub(crate) fn phase_terminal(letter: &str) -> Option<&'static str> {
    match letter {
        "A" | "s1" => Some("1"),
        "B" | "s2" => Some("2"),
        "C" => Some("3"),
        "N" => Some("4"),
        _ => None,
    }
}

/// The CIM phase letter for a terminal name, the writer-side inverse of
/// [`phase_terminal`] (split-phase collapses onto A/B).
pub(crate) fn terminal_phase(terminal: &str) -> Option<&'static str> {
    match terminal {
        "1" => Some("A"),
        "2" => Some("B"),
        "3" => Some("C"),
        "4" => Some("N"),
        _ => None,
    }
}

/// Expand a `PhaseCode.<code>` / `OrderedPhaseCodeKind.<code>` value into
/// per-phase letters: `ABCN` → A, B, C, N; `s1s2N` → s1, s2, N.
pub(crate) fn expand_phase_code(code: &str) -> Vec<String> {
    let code = code.rsplit('.').next().unwrap_or(code);
    let mut out = Vec::new();
    let mut chars = code.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            'A' | 'B' | 'C' | 'N' => out.push(c.to_string()),
            's' => {
                if let Some(d) = chars.next() {
                    out.push(format!("s{d}"));
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn det_mrid_is_uuid_shaped_and_stable() {
        let a = det_mrid("ACLineSegment", "ohline");
        assert_eq!(a, det_mrid("ACLineSegment", "ohline"));
        assert_ne!(a, det_mrid("ACLineSegment", "quad"));
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "version nibble: {a}");
    }

    #[test]
    fn phase_codes_expand_in_order() {
        assert_eq!(expand_phase_code("PhaseCode.ABCN"), ["A", "B", "C", "N"]);
        assert_eq!(expand_phase_code("OrderedPhaseCodeKind.CA"), ["C", "A"]);
        assert_eq!(expand_phase_code("PhaseCode.s1s2N"), ["s1", "s2", "N"]);
        assert_eq!(expand_phase_code("A"), ["A"]);
    }

    #[test]
    fn natural_keys_order_numerically() {
        let mut names = vec!["line10", "line2", "line1"];
        names.sort_by_key(|n| natural_key(n));
        assert_eq!(names, ["line1", "line2", "line10"]);
    }
}
