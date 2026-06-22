//! Validates cross-implementation test vector registry.
//! Run: cargo test hip23_test_vectors_ -- --nocapture

use std::path::PathBuf;

#[test]
fn hip23_test_vectors_registry_loads() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hip23_test_vectors.json");
    let raw = std::fs::read_to_string(&path).expect("hip23_test_vectors.json missing");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("invalid JSON");
    assert_eq!(v["version"], "1.0");
    assert_eq!(v["spec"], "HIP-23");
    let vectors = v["vectors"].as_array().expect("vectors array");
    assert!(vectors.len() >= 12, "expected >= 12 vectors, got {}", vectors.len());
    for entry in vectors {
        assert!(entry["id"].is_string());
        assert!(entry["pattern"].is_string());
        assert!(entry["test"].is_string());
        assert!(entry["expect"].is_string());
    }
}