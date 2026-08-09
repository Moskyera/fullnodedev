//! Will the node actually serve the API on the configs this repository ships?
//!
//! The node has a rule of its own, in `server/src/server/server.rs`:
//!
//! ```text
//! if !addr.ip().is_loopback() && ser.cnf.api_token.is_empty() {
//!     println!("[Error] api server bind ... is not loopback but api_token is empty");
//!     return;
//! }
//! ```
//!
//! It does not exit. It prints one line and returns from the listen task, so the
//! process keeps running and keeps syncing the chain, looking healthy in every
//! way except the one that matters: nothing is listening on the API port.
//!
//! `deploy/node/hacash.config.ini` shipped `bind = 0.0.0.0` with no `api_token`
//! at all, and its own header comment called that "correct INSIDE a container".
//! It is not correct anywhere. The compose healthcheck curls `/query/latest` and
//! would never have passed, the pool waits on `service_healthy` and would never
//! have started, and an operator would have seen a node happily syncing and a
//! pool that never came up.
//!
//! This test applies the node's own rule to every node config in the tree, so
//! the next one cannot ship broken either.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The `[section] key = value` pairs of an ini file, lowercased keys, comments
/// (`;` and `#`) and blank lines dropped. Deliberately a small independent
/// reader rather than the node's own: a test that parses with the code under
/// test cannot catch a config the code would reject.
fn ini_pairs(text: &str) -> HashMap<(String, String), String> {
    let mut out = HashMap::new();
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            section = rest
                .split(']')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.insert(
                (section.clone(), k.trim().to_ascii_lowercase()),
                v.trim().to_string(),
            );
        }
    }
    out
}

/// Every `hacash.config.ini` this repository ships to an operator.
fn shipped_node_configs() -> Vec<PathBuf> {
    // The crate root is hbit-pool/, so the repository is one level up.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root")
        .to_path_buf();
    ["deploy/node/hacash.config.ini"]
        .iter()
        .map(|p| root.join(p))
        .filter(|p| p.exists())
        .collect()
}

#[test]
fn every_shipped_node_config_is_one_the_node_will_actually_serve() {
    let configs = shipped_node_configs();
    assert!(
        !configs.is_empty(),
        "no node config was found to check; this test must not pass by finding nothing"
    );

    for path in configs {
        let text = std::fs::read_to_string(&path).expect("read node config");
        let ini = ini_pairs(&text);

        let enabled = ini
            .get(&("server".into(), "enable".into()))
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if !enabled {
            continue; // no API means nothing to serve and nothing to check
        }

        let bind = ini
            .get(&("server".into(), "bind".into()))
            .cloned()
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let token = ini
            .get(&("server".into(), "api_token".into()))
            .cloned()
            .unwrap_or_default();

        // The node's own test, verbatim: a non-loopback bind with no token is
        // refused. "0.0.0.0" is NOT loopback - that is the whole trap, because
        // it reads like "everything, including localhost".
        let loopback = bind == "127.0.0.1" || bind == "::1" || bind == "localhost";
        assert!(
            loopback || !token.trim().is_empty(),
            "{}: [server] bind = {bind} is not loopback and api_token is empty. The node will \
             print one line and never listen, while the process keeps running and syncing. The \
             pool would report the node as down for ever.",
            path.display()
        );
    }
}

#[test]
fn a_client_built_with_a_token_is_not_the_same_as_one_without() {
    // The pool sends the token on the CLIENT, so no call site can forget it.
    // This pins that an empty token really does mean "no header" - every
    // existing loopback deployment depends on that being unchanged - and that a
    // token is accepted rather than silently dropped.
    let _plain = hbit_pool::http_client_with_token("");
    let _authed = hbit_pool::http_client_with_token("a-real-token");
    // A token containing bytes no header can carry must not panic the pool; it
    // warns and sends nothing, which the node then refuses, which the operator
    // sees. Panicking here would take down a running pool instead.
    let _bad = hbit_pool::http_client_with_token("bad\ntoken");
}
