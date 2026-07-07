//! MG-RAVENS integration: JSON auto-detection through the hub, the vendored
//! upstream multiconductor example's refusal, and the CLI-visible token.

use std::path::PathBuf;

use powerio::{TargetFormat, convert_file, parse_file, target_format_from_name};

fn data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/data")
        .join(name)
}

#[test]
fn ravens_json_is_sniffed_from_bare_json() {
    let dir = std::env::temp_dir().join("powerio-ravens-sniff");
    std::fs::create_dir_all(&dir).unwrap();
    let out = convert_file(data("case9.m"), TargetFormat::RavensJson, None).unwrap();
    let path = dir.join("case9.ravens.json");
    std::fs::write(&path, &out.text).unwrap();

    // No `from` hint: the `.json` classifier must land on MG-RAVENS.
    let parsed = parse_file(&path, None).unwrap();
    assert_eq!(parsed.network.buses.len(), 9);
    assert_eq!(parsed.network.generators.len(), 3);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn upstream_multiconductor_example_is_refused_with_guidance() {
    let err = parse_file(data("ravens/case3_balanced.json"), Some("ravens-json")).unwrap_err();
    let message = err.to_string();
    assert!(message.contains("multiconductor"), "got: {message}");
    assert!(message.contains("powerio-dist"), "got: {message}");

    // The sniffing path refuses it identically rather than misrouting it.
    let err = parse_file(data("ravens/case3_balanced.json"), None).unwrap_err();
    assert!(err.to_string().contains("multiconductor"), "got: {err}");
}

#[test]
fn ravens_aliases_resolve() {
    for alias in ["ravens-json", "ravens", "mg-ravens", "RavensJson"] {
        assert_eq!(
            target_format_from_name(alias),
            Some(TargetFormat::RavensJson),
            "{alias}"
        );
    }
}
