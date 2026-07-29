//! Cross-platform binary names: Windows keeps `.exe` first (unchanged behavior).

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn configure_background_command(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

/// Give the child its OWN console window instead of hiding it.
///
/// For the full node and the miners, which an operator has every reason to watch:
/// a node downloading the chain prints its progress, and a panel that hides that
/// leaves the user staring at a button with no idea whether anything is
/// happening. That is not hypothetical. A node stuck part way through a sync
/// looked identical, from the panel, to one that was ready.
///
/// The trade is real and worth stating: a process with its own console is NOT
/// piping its output back here. Status still works, because it comes from
/// polling the node's RPC and from the child's exit code, neither of which needs
/// the pipe.
///
/// For the two miners the pipe is no longer the only copy either: they write
/// every line they print to a rolling log beside their config, and the panel
/// tails it (`app::worker_log` and `worker_log_tail`). For the node it still is,
/// because the node's output comes from crates that cannot depend on `app`.
pub fn configure_visible_command(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        command.creation_flags(CREATE_NEW_CONSOLE);
    }
    #[cfg(not(windows))]
    {
        // No equivalent: on Linux and macOS a GUI-launched child has no terminal
        // to attach to, and spawning one would mean picking a terminal emulator
        // that may not be installed. The panel keeps the piped log there.
        let _ = command;
    }
}
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

/// Best-effort guard against launching a second node on the same database.
pub fn fullnode_process_running() -> bool {
    if cfg!(windows) {
        return fullnode_names().iter().any(|name| {
            let mut command = Command::new("tasklist");
            configure_background_command(&mut command);
            command
                .args(["/FI", &format!("IMAGENAME eq {name}"), "/FO", "CSV", "/NH"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).to_ascii_lowercase())
                .map(|out| out.contains(&format!("\"{}\"", name.to_ascii_lowercase())))
                .unwrap_or(false)
        });
    }

    ["hacash", "fullnode"].iter().any(|name| {
        Command::new("pgrep")
            .args(["-x", name])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}
