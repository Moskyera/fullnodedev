//! The config files that ship with a release are the defaults every operator
//! who never opens the GUI actually runs on. They are text, so nothing compiles
//! them and nothing checked them; this does.
//!
//! Two things are checked, and both of them shipped broken:
//!
//! 1. A HACD config must not pin a thread count. A number in a file is a guess
//!    about a machine the file has never seen. The one that shipped was 6,
//!    which on the measured 16-core / 32-thread CPU is 320,097 H/s against
//!    1,442,210 for the count the worker now derives: 22% of the machine.
//!
//! 2. No config may say `dynamic_supervene = true` next to `supervene_max = 0`.
//!    `EfficiencyConf::spawn_supervene` ignores the flag entirely unless the cap
//!    is above zero, so that pair is a setting that reads as on and is off.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("app/ has a parent")
        .to_path_buf()
}

/// Flat key/value view of an ini, section names ignored. Every key checked here
/// is unique across the file, and reading it flat means a key that moves between
/// `[efficiency]` and the root still gets checked.
fn ini_pairs(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(';') || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.insert(
                k.trim().to_ascii_lowercase(),
                v.split(';').next().unwrap_or(v).trim().to_string(),
            );
        }
    }
    out
}

/// The `.bat` setup scripts write their default config with `echo key = value`
/// lines, so the same reader works on them once the `echo` is stripped.
fn bat_generated_pairs(text: &str, label: &str) -> HashMap<String, String> {
    let body: String = text
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            t.strip_prefix("echo ").map(|rest| rest.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n");
    let pairs = ini_pairs(&body);
    assert!(
        pairs.contains_key("supervene"),
        "{label} writes no supervene line at all"
    );
    pairs
}

fn push_ini_files(dir: PathBuf, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.ends_with(".ini") || name.ends_with(".ini.example") {
            files.push(path);
        }
    }
}

/// Every ini or ini-shaped file a release hands an operator.
fn shipped_config_files() -> Vec<PathBuf> {
    let root = repo_root();
    let mut files = Vec::new();
    for dir in [
        "mainnet-configs",
        "scripts/mining-amd",
        "scripts/mining-nvidia",
        "scripts/mining-amd/presets/diaworker",
        "scripts/mining-amd/presets/poworker",
    ] {
        push_ini_files(root.join(dir), &mut files);
    }
    files.sort();
    files
}

/// A flag that reads as on and is off is worse than one that is off: an
/// operator who wanted dynamic CPU assist ticked it, got nothing, and had no
/// way to find out.
#[test]
fn no_shipped_config_claims_dynamic_supervene_it_cannot_perform() {
    let files = shipped_config_files();
    assert!(
        files.len() >= 20,
        "expected to find the shipped configs, found {}",
        files.len()
    );
    let mut checked = 0usize;
    for path in &files {
        let text = std::fs::read_to_string(path).expect("readable");
        let pairs = ini_pairs(&text);
        let Some(dynamic) = pairs.get("dynamic_supervene") else {
            continue;
        };
        if !matches!(dynamic.as_str(), "true" | "1" | "yes") {
            continue;
        }
        checked += 1;
        let max: u32 = pairs
            .get("supervene_max")
            .map(|v| v.parse().unwrap_or(0))
            .unwrap_or(0);
        assert!(
            max > 0,
            "{}: dynamic_supervene = true with supervene_max = {max}. \
             spawn_supervene ignores the flag unless the cap is above zero, \
             so this setting does nothing.",
            path.display()
        );
    }
    assert!(
        checked > 0,
        "no shipped config enables dynamic_supervene, so this test proved nothing"
    );
}

/// The HACD worker owns the whole CPU, and the right number is a property of the
/// machine. A file cannot know it, so a shipped HACD config must decline to
/// guess: `supervene = 0` is read by `DiaWorkConf::new` as "fit this machine".
///
/// The per-machine presets under `scripts/mining-amd/presets/` are excluded on
/// purpose: naming a CPU in the filename is exactly the case where a number IS
/// knowledge rather than a guess.
#[test]
fn shipped_hacd_configs_do_not_pin_a_thread_count() {
    let root = repo_root();
    let generic = [
        root.join("mainnet-configs/diaworker.mainnet.ini"),
        root.join("scripts/mining-amd/diaworker.amd.ini.example"),
    ];
    for path in &generic {
        let text =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let pairs = ini_pairs(&text);
        let sv = pairs
            .get("supervene")
            .unwrap_or_else(|| panic!("{}: no supervene key", path.display()));
        assert_eq!(
            sv,
            "0",
            "{}: ships supervene = {sv}. A generic HACD config must not pin a \
             thread count; 0 means the worker counts this machine's cores.",
            path.display()
        );
    }

    for (name, label) in [
        ("SETUP.bat", "SETUP.bat"),
        ("SETUP-MINER.bat", "SETUP-MINER.bat"),
    ] {
        let text = std::fs::read_to_string(root.join(name)).expect("readable");
        // Only the diaworker generator: the poworker one in the same file writes
        // its own supervene and is a different question. The label is matched at
        // the start of a line, because `call :write_default_diaworker_ini`
        // appears earlier in the file and is not the subroutine.
        let start = text
            .find("\n:write_default_diaworker_ini")
            .unwrap_or_else(|| panic!("{label}: no diaworker generator"));
        let block = &text[start..];
        let end = block.find("exit /b 0").unwrap_or(block.len());
        let pairs = bat_generated_pairs(&block[..end], label);
        assert_eq!(
            pairs.get("supervene").map(String::as_str),
            Some("0"),
            "{label} writes a pinned HACD thread count: {:?}",
            pairs.get("supervene")
        );
        assert_eq!(
            pairs.get("dynamic_supervene").map(String::as_str),
            Some("false"),
            "{label} writes dynamic_supervene for a worker with no GPU to balance against"
        );
    }
}

/// What the shipped file means, read through the code that reads it. The ini
/// text and `DiaWorkConf` are two halves of one decision, and this is the seam
/// where a rename or a changed default would part them without any test failing.
#[test]
fn the_shipped_hacd_config_resolves_to_this_machines_thread_count() {
    let path = repo_root().join("mainnet-configs/diaworker.mainnet.ini");
    assert!(path.is_file(), "{} is missing", path.display());
    let ini = sys::load_config_path(&path);
    // `load_config_path` answers an unreadable file with an EMPTY ini, and an
    // empty ini would satisfy every assertion below for the wrong reason.
    assert_eq!(
        sys::ini_must(&sys::ini_section(&ini, "default"), "connect", ""),
        "127.0.0.1:8080",
        "the shipped config did not parse, so nothing below was really tested"
    );
    let cnf = app::diaworker::DiaWorkConf::new(&ini);

    let expected = app::cpu_threads::hacd_threads();
    assert_eq!(cnf.supervene, expected);
    assert_eq!(cnf.efficiency.clamp_supervene(cnf.supervene), expected);
    assert_eq!(cnf.efficiency.spawn_supervene(cnf.supervene), expected);

    let logical = app::cpu_threads::logical_cpus();
    if logical > app::cpu_threads::HOST_RESERVE_THREADS {
        assert_eq!(
            cnf.supervene,
            logical - app::cpu_threads::HOST_RESERVE_THREADS,
            "the shipped config must leave exactly the host reserve free"
        );
    }
    // The number that used to ship, and the number the panel used to offer.
    // Both are only correct on a machine that happens to have that many cores.
    if logical >= 16 {
        assert_ne!(cnf.supervene, 6);
        assert_ne!(cnf.supervene, 8);
    }
    assert!(!cnf.useopencl, "HACD stays CPU-only");
}
