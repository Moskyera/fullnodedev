//! A rolling log file the miners write beside their config, plus the two macros
//! that fill it.
//!
//! Why it exists. On Windows the panel now gives the node and every worker their
//! OWN console window. That was the right call for the operator, who can finally
//! watch the work, but a child with its own console pipes nothing back: the log
//! channel the panel gets from `spawn_worker_with_logs` stays empty there, so the
//! event feed and the log view had nothing to show at all.
//!
//! The answer is both. The console output is untouched, byte for byte; this is a
//! second copy of the same lines, written where the panel can read them.
//!
//! Bounded on purpose. A miner left running for a month must not fill a disk, so
//! the live file is capped and rotated exactly once. At most `2 * max_bytes` of
//! log exists at any moment, and that is a ceiling, not an average.
//!
//! Nothing here may take a miner down. Every failure is reported once on stderr
//! and then swallowed until it recovers: losing the log is never a reason to stop
//! hashing.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard};

/// Ceiling for the live file. One rotation is kept beside it, so the worst case
/// on disk is twice this.
pub const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Refuse to run with a cap so small that rotation would thrash. Also what makes
/// the bound meaningful for tests, which set a tiny cap on purpose.
const MIN_MAX_BYTES: u64 = 4 * 1024;

/// Longest single entry kept. One enormous line cannot overshoot the file bound
/// by more than this.
const MAX_ENTRY_BYTES: usize = 8 * 1024;

/// Set while a log file is open. Read on every printed line, so it is an atomic
/// rather than a lock: a worker that never called `init` pays one relaxed load.
static ACTIVE: AtomicBool = AtomicBool::new(false);
static LOG: Mutex<Option<Rolling>> = Mutex::new(None);
/// True while a write failure is being suppressed, so a failing disk produces
/// one line on stderr instead of one per print.
static WARNED: AtomicBool = AtomicBool::new(false);

/// Recover from a poisoned lock rather than propagate the panic. The worst case
/// is one lost log line.
fn lock() -> MutexGuard<'static, Option<Rolling>> {
    LOG.lock().unwrap_or_else(|e| e.into_inner())
}

/// The log that belongs to a worker: `<dir>/<stem>.log`.
///
/// Shared with the panel (`miner-panel` tails exactly this path) so the writer
/// and the reader can never disagree about the name.
pub fn log_path(dir: &Path, stem: &str) -> PathBuf {
    dir.join(format!("{stem}.log"))
}

/// The path a rotation moves the live file to.
pub fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

/// Start logging to `<config dir>/<stem>.log`. Returns the path so the caller
/// can say where it went.
pub fn init_beside_config(config_path: &Path, stem: &str) -> Result<PathBuf, String> {
    let dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let path = log_path(dir, stem);
    init_with_limit(&path, DEFAULT_MAX_BYTES)?;
    Ok(path)
}

/// Start logging to an exact path with an exact cap. The whole file is one
/// global because a worker has one log; calling this again replaces it.
pub fn init_with_limit(path: &Path, max_bytes: u64) -> Result<(), String> {
    let rolling = Rolling::open(path, max_bytes.max(MIN_MAX_BYTES))
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    *lock() = Some(rolling);
    WARNED.store(false, Relaxed);
    ACTIVE.store(true, Relaxed);
    Ok(())
}

/// Stop logging and close the file. Used by the tests; a worker just exits.
pub fn shutdown() {
    ACTIVE.store(false, Relaxed);
    *lock() = None;
}

pub fn is_active() -> bool {
    ACTIVE.load(Relaxed)
}

/// Append one printed line. A multi-line argument becomes one entry per line, so
/// the panel never has to reassemble anything.
pub fn record(text: &str) {
    if !ACTIVE.load(Relaxed) {
        return;
    }
    let stamp = sys::ctshow();
    let mut guard = lock();
    let Some(log) = guard.as_mut() else {
        return;
    };
    for part in text.split('\n') {
        if let Err(error) = log.write_entry(&stamp, part.trim_end_matches('\r')) {
            drop(guard);
            warn_once(&error.to_string());
            return;
        }
    }
    WARNED.store(false, Relaxed);
}

fn warn_once(error: &str) {
    if !WARNED.swap(true, Relaxed) {
        eprintln!("[log] cannot write the worker log ({error}); suppressing until it recovers");
    }
}

struct Rolling {
    path: PathBuf,
    rotated: PathBuf,
    /// `None` only for the instant a rotation holds, so the old file is closed
    /// before it is renamed.
    file: Option<File>,
    written: u64,
    max_bytes: u64,
}

impl Rolling {
    fn open(path: &Path, max_bytes: u64) -> io::Result<Self> {
        let file = open_append(path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mut rolling = Rolling {
            path: path.to_path_buf(),
            rotated: rotated_path(path),
            file: Some(file),
            written,
            max_bytes,
        };
        // A previous run may have left the file at the cap. Roll it now rather
        // than resume above the bound.
        if rolling.written >= rolling.max_bytes {
            rolling.rotate()?;
        }
        Ok(rolling)
    }

    fn write_entry(&mut self, stamp: &str, text: &str) -> io::Result<()> {
        let mut entry = String::with_capacity(stamp.len() + text.len() + 2);
        entry.push_str(stamp);
        entry.push(' ');
        push_bounded(&mut entry, text);
        entry.push('\n');

        let len = entry.len() as u64;
        if self.written > 0 && self.written + len > self.max_bytes {
            self.rotate()?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("the worker log file is closed"))?;
        // `std::fs::File` is unbuffered, so this reaches the OS on the spot.
        // That is what lets the panel see a line within one poll instead of
        // whenever a buffer happened to fill.
        file.write_all(entry.as_bytes())?;
        self.written += len;
        Ok(())
    }

    /// Move the live file aside and start a new one. The cap is what matters: if
    /// the rename cannot be done, the old file is dropped rather than allowed to
    /// keep growing.
    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        let _ = fs::rename(&self.path, &self.rotated);
        // Truncating, not appending: after a failed rename the file is still
        // there, and reopening it in append mode would defeat the bound.
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)?,
        );
        self.written = 0;
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Append `text`, cut to `MAX_ENTRY_BYTES` on a character boundary.
fn push_bounded(out: &mut String, text: &str) {
    if text.len() <= MAX_ENTRY_BYTES {
        out.push_str(text);
        return;
    }
    let mut end = MAX_ENTRY_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    out.push_str(&text[..end]);
    out.push_str("...");
}

/// `println!`, plus a copy in the worker's rolling log.
///
/// The console output is identical to what `println!` produced before, which is
/// the whole point: the operator's console window is not being changed, it is
/// being mirrored.
#[macro_export]
macro_rules! wlogln {
    () => {{
        std::println!();
        $crate::worker_log::record("");
    }};
    ($($arg:tt)*) => {{
        let line = std::format!($($arg)*);
        std::println!("{}", line);
        $crate::worker_log::record(&line);
    }};
}

/// `eprintln!`, plus a copy in the worker's rolling log.
#[macro_export]
macro_rules! wlogerr {
    ($($arg:tt)*) => {{
        let line = std::format!($($arg)*);
        std::eprintln!("{}", line);
        $crate::worker_log::record(&line);
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log is one process-wide file, so these tests take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn scratch(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "hacash-worker-log-{}-{name}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn the_log_lands_beside_the_config_under_the_worker_name() {
        // The panel computes the same two paths to tail them, so they are fixed
        // here rather than left to agree by accident.
        let dir = Path::new("C:/miner");
        assert_eq!(log_path(dir, "poworker"), dir.join("poworker.log"));
        assert_eq!(
            rotated_path(&log_path(dir, "diaworker")),
            dir.join("diaworker.log.1")
        );
    }

    #[test]
    fn every_printed_line_reaches_the_file_without_waiting_for_a_buffer() {
        let _serial = serial();
        let dir = scratch("write");
        let path = log_path(&dir, "poworker");
        init_with_limit(&path, DEFAULT_MAX_BYTES).expect("open the log");

        record("[Mining] first");
        // A multi-line print becomes one entry per line: the panel splits on
        // newlines and must never see a half entry.
        record("[Mining] second\r\n[Mining] third");

        let text = fs::read_to_string(&path).expect("read back");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "got {text:?}");
        assert!(lines[0].ends_with(" [Mining] first"), "{}", lines[0]);
        assert!(lines[1].ends_with(" [Mining] second"), "{}", lines[1]);
        assert!(lines[2].ends_with(" [Mining] third"), "{}", lines[2]);
        // Every entry carries the time it was printed, so the panel shows a
        // measured timestamp instead of the moment it happened to read the file.
        assert_eq!(lines[0].chars().nth(4), Some('-'), "{}", lines[0]);

        shutdown();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_miner_left_running_cannot_grow_the_log_past_two_files() {
        let _serial = serial();
        let dir = scratch("rotate");
        let path = log_path(&dir, "poworker");
        let cap = MIN_MAX_BYTES;
        init_with_limit(&path, cap).expect("open the log");

        // Far more than the cap, in small lines, the way a worker really writes.
        for i in 0..4_000 {
            record(&format!("[Mining] batch {i} of a very long night"));
        }

        let live = fs::metadata(&path).expect("live file").len();
        let rolled = fs::metadata(rotated_path(&path))
            .expect("one rotation is kept")
            .len();
        assert!(live <= cap, "live file {live} is over the {cap} byte cap");
        assert!(rolled <= cap, "rotated file {rolled} is over the cap");
        // And nothing else: exactly two files, whatever the miner printed.
        let files = fs::read_dir(&dir).expect("dir").count();
        assert_eq!(files, 2, "only the live log and one rotation may exist");
        // The newest lines survived the rotation.
        let text = fs::read_to_string(&path).expect("read back");
        assert!(text.contains("batch 3999"), "the last line must be kept");

        shutdown();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_worker_that_never_opened_a_log_just_prints() {
        let _serial = serial();
        shutdown();
        assert!(!is_active());
        record("[Mining] nowhere to go");
        // Nothing to assert but the absence of a panic: this is the path every
        // test binary and the benchmark tools take.
    }

    #[test]
    fn one_enormous_line_cannot_overshoot_the_bound() {
        let _serial = serial();
        let dir = scratch("huge");
        let path = log_path(&dir, "poworker");
        init_with_limit(&path, MIN_MAX_BYTES).expect("open the log");

        record(&"x".repeat(MAX_ENTRY_BYTES * 4));

        let live = fs::metadata(&path).expect("live file").len();
        assert!(
            live <= MIN_MAX_BYTES + MAX_ENTRY_BYTES as u64 + 64,
            "a single line wrote {live} bytes"
        );

        shutdown();
        let _ = fs::remove_dir_all(&dir);
    }
}
