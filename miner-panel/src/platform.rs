//! Cross-platform binary names — Windows keeps `.exe` first (unchanged behavior).

use std::path::{Path, PathBuf};

/// Candidate filenames for a worker/fullnode binary (platform-preferred order).
pub fn binary_names(stem: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![format!("{stem}.exe"), stem.to_string()]
    } else {
        vec![stem.to_string(), format!("{stem}.exe")]
    }
}

pub fn fullnode_names() -> Vec<String> {
    let mut names = Vec::new();
    for stem in ["hacash", "fullnode"] {
        for n in binary_names(stem) {
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    names
}

/// Resolve binary next to panel / target/release (same search paths as before).
pub fn find_binary(work_dir: &Path, names: &[String]) -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for name in names {
        candidates.push(work_dir.join(name));
        candidates.push(work_dir.join("..").join(name));
        candidates.push(
            work_dir
                .join("..")
                .join("..")
                .join("target")
                .join("release")
                .join(name),
        );
        candidates.push(
            work_dir
                .join("..")
                .join("..")
                .join("target")
                .join("debug")
                .join(name),
        );
    }
    for c in candidates {
        if c.is_file() {
            return c.canonicalize().unwrap_or(c);
        }
    }
    work_dir.join(&names[0])
}

pub fn find_worker(work_dir: &Path, stem: &str) -> PathBuf {
    find_binary(work_dir, &binary_names(stem))
}

pub fn find_fullnode(work_dir: &Path) -> PathBuf {
    find_binary(work_dir, &fullnode_names())
}

/// UI label: `poworker.exe` on Windows, `poworker` on Linux.
pub fn bin_label(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}