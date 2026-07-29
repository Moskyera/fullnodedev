//! The reader half of the worker's rolling log.
//!
//! On Windows the panel gives each worker its own console window, which is what
//! the operator wanted, and the price is that the child pipes nothing back: the
//! channel `spawn_worker_with_logs` returns is empty there. So the workers also
//! write every line they print to `<worker>.log` beside their config
//! (`app::worker_log`), and this tails that file.
//!
//! Two rules shape it.
//!
//! It never reads the whole file. It remembers the byte offset it stopped at and
//! reads forward from there, at most a bounded slice per poll, so a frame costs
//! the same whether the log is 4 KiB or 4 MiB.
//!
//! It never replays. Tailing starts at the length the file had when the panel
//! launched the worker, so lines from yesterday's run are not shown as if they
//! had just happened. That matters because the event feed stamps a line with the
//! time the panel read it: replaying old text would put an invented time on it.
//!
//! The ring of lines is kept apart from the tail because the Logs screen must
//! work on every platform and the file is only the source on one of them.
//! Everywhere else the same lines arrive on the child's pipe, and `record_piped`
//! puts them in the same ring. So the screen has one source, and it is whichever
//! one this platform actually measured; nothing is read twice, because a child
//! with its own console (Windows) pipes nothing and a piped child is not tailed.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How many lines the panel keeps in memory for the log view. Public because
/// the Logs screen prints it beside the number it is actually holding, and a
/// hard-coded ceiling there would start lying the day this changes.
pub const MAX_LINES: usize = 500;
/// Longest line kept. The workers print a few hundred characters at most; this
/// only stops a corrupted file from pushing a megabyte into the UI.
const MAX_LINE_CHARS: usize = 400;
/// Most bytes read in one poll. A backlog is not dropped, it is read over the
/// next few polls, so this bounds the work per frame and nothing else.
const MAX_READ_PER_POLL: u64 = 128 * 1024;
/// A run of bytes with no newline in it is a half written line, not a line. If
/// it ever grows past this the file is not what we think it is, so it is flushed
/// as one line rather than buffered without limit.
const MAX_PARTIAL_BYTES: usize = 64 * 1024;
/// The log is polled on this cadence, not every frame: the panel repaints far
/// faster than a miner prints.
const POLL_INTERVAL: Duration = Duration::from_millis(300);
/// `2026-07-29 14:03:11` is what `app::worker_log` writes in front of every
/// entry, and it is always exactly this wide.
const STAMP_LEN: usize = 19;

/// One line the worker printed: the time it printed it, and what it said.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    /// The worker's own timestamp, or empty for a line that carried none.
    pub stamp: String,
    /// The line as the worker printed it, with the timestamp taken back off, so
    /// it can be classified by the same rules as a piped line.
    pub text: String,
}

#[derive(Default)]
struct Tail {
    path: PathBuf,
    /// Where reading stopped. Bytes, from the start of the file.
    offset: u64,
    /// The tail of the last read, if it did not end on a newline.
    partial: String,
    next_poll: Option<Instant>,
    /// False once the worker has stopped. What was read stays readable; nothing
    /// new is looked for.
    active: bool,
}

static TAIL: Mutex<Option<Tail>> = Mutex::new(None);
/// Every line the panel has read this session, whichever source it came from.
/// Separate from `Tail` because the pipe fills it on the platforms where the
/// file is not read, and because it must survive a stopped worker.
static LINES: Mutex<VecDeque<Line>> = Mutex::new(VecDeque::new());

/// A poisoned lock costs one poll, not the panel.
fn lock() -> MutexGuard<'static, Option<Tail>> {
    TAIL.lock().unwrap_or_else(|e| e.into_inner())
}

fn lines() -> MutexGuard<'static, VecDeque<Line>> {
    LINES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Add one line to the ring, oldest dropped first.
///
/// Both sources come through here, so the bound is one rule in one place. It is
/// never called with a line the panel did not read from a running worker.
fn keep(line: Line) {
    let mut ring = lines();
    ring.push_back(line);
    while ring.len() > MAX_LINES {
        ring.pop_front();
    }
}

/// Begin tailing `path` for a worker that is about to start.
///
/// Call this BEFORE spawning, and only then: the offset is taken from the file
/// as it stands now, so everything the new worker appends is read and nothing
/// the previous run left behind is read twice.
///
/// Restarting the same worker keeps the lines already read, so an automatic
/// recovery does not wipe the log of the run that failed.
pub fn start(path: &Path) {
    let offset = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut guard = lock();
    // A different worker is a different log. Keeping the old one under the new
    // worker's name would attribute one program's output to another.
    if guard.as_ref().is_none_or(|tail| tail.path != path) {
        lines().clear();
    }
    *guard = Some(Tail {
        path: path.to_path_buf(),
        offset,
        partial: String::new(),
        next_poll: None,
        active: true,
    });
}

/// Stop looking for new lines. What has been read stays readable: a miner who
/// has just stopped a worker is exactly the miner who wants to see its log.
pub fn stop() {
    if let Some(tail) = lock().as_mut() {
        tail.active = false;
    }
}

/// Let the next `poll` read straight away instead of waiting out the interval.
///
/// For the one moment the wait is wrong: the worker has just exited, this is the
/// last chance to read it, and what it printed on the way out is the reason.
pub fn read_immediately() {
    if let Some(tail) = lock().as_mut() {
        tail.next_poll = None;
    }
}

/// Read whatever the worker has appended since the last call, oldest first.
///
/// Returns an empty vector when the poll interval has not elapsed, when there is
/// nothing new, or when there is no log to read; none of those are errors and
/// none of them are reported. A worker whose log cannot be read still mines, and
/// the panel still has the stats file and the exit code.
// Only the platform whose children own a console reads the file: everywhere else
// the same lines arrive on the pipe, and taking both would show each one twice.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn poll() -> Vec<Line> {
    let mut guard = lock();
    let Some(tail) = guard.as_mut().filter(|tail| tail.active) else {
        return Vec::new();
    };
    let now = Instant::now();
    if tail.next_poll.is_some_and(|next| now < next) {
        return Vec::new();
    }
    tail.next_poll = Some(now + POLL_INTERVAL);
    tail.read_new()
}

/// Keep one line that arrived on the child's pipe rather than through the file.
///
/// That is the live source on every platform whose workers are piped. On Windows
/// the pipe is empty by construction, so calling this for whatever the channel
/// yielded costs nothing there and needs no `cfg`.
///
/// The line carries no timestamp of its own: it is the raw text the worker
/// printed, and the panel does not put its own clock on it.
pub fn record_piped(text: &str) {
    let line = parse_line(text);
    if line.text.is_empty() {
        return;
    }
    keep(line);
}

/// The newest lines the panel has read, newest first. This is the Logs screen's
/// source: the bounded ring, already parsed, no file access at all.
pub fn recent(count: usize) -> Vec<Line> {
    let ring = lines();
    ring.iter().rev().take(count).cloned().collect()
}

/// How many lines are held. The Logs screen prints this so the ceiling is
/// visible rather than implied.
pub fn line_count() -> usize {
    lines().len()
}

/// The rolling log file the current worker writes, once one has been launched.
///
/// It is the file the panel tails on Windows and the file the worker writes
/// everywhere; the Logs screen shows it so an operator can open the full history
/// themselves, which is more than the bounded ring holds.
pub fn source_path() -> Option<PathBuf> {
    lock().as_ref().map(|tail| tail.path.clone())
}

impl Tail {
    fn read_new(&mut self) -> Vec<Line> {
        let Ok(mut file) = File::open(&self.path) else {
            return Vec::new();
        };
        let Ok(len) = file.metadata().map(|m| m.len()) else {
            return Vec::new();
        };
        if len < self.offset {
            // The worker rotated the file, or something replaced it. The file
            // that is there now starts at the beginning, and it is the one with
            // the recent lines in it.
            self.offset = 0;
            self.partial.clear();
        }
        let available = len - self.offset;
        if available == 0 {
            return Vec::new();
        }
        let want = available.min(MAX_READ_PER_POLL);
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = vec![0u8; want as usize];
        let read = match file.read(&mut buf) {
            Ok(read) => read,
            Err(_) => return Vec::new(),
        };
        if read == 0 {
            return Vec::new();
        }
        self.offset += read as u64;

        // Lossy on purpose: a torn multi-byte character at the end of a read is
        // normal, and it must not cost the whole batch.
        let chunk = String::from_utf8_lossy(&buf[..read]);
        let mut fresh = Vec::new();
        for piece in chunk.split_inclusive('\n') {
            self.partial.push_str(piece);
            if !piece.ends_with('\n') {
                if self.partial.len() > MAX_PARTIAL_BYTES {
                    fresh.push(parse_line(&std::mem::take(&mut self.partial)));
                }
                continue;
            }
            let complete = std::mem::take(&mut self.partial);
            let line = parse_line(&complete);
            if line.text.is_empty() {
                continue;
            }
            fresh.push(line);
        }

        for line in &fresh {
            keep(line.clone());
        }
        fresh
    }
}

/// Split one written entry into the worker's timestamp and its own words.
///
/// A line without the timestamp keeps all of its text and gets no stamp, rather
/// than having its first 19 characters eaten on a guess.
fn parse_line(raw: &str) -> Line {
    let trimmed = raw.trim_end_matches(['\n', '\r']);
    let (stamp, rest) = match trimmed.split_at_checked(STAMP_LEN) {
        Some((head, rest)) if is_stamp(head) && rest.starts_with(' ') => (head, &rest[1..]),
        _ => ("", trimmed),
    };
    Line {
        stamp: stamp.to_string(),
        text: clamp(rest),
    }
}

/// `2026-07-29 14:03:11`, and nothing that merely looks close.
fn is_stamp(head: &str) -> bool {
    let bytes = head.as_bytes();
    if bytes.len() != STAMP_LEN {
        return false;
    }
    const DIGITS: [usize; 14] = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    const DASHES: [usize; 2] = [4, 7];
    const COLONS: [usize; 2] = [13, 16];
    DIGITS.iter().all(|&i| bytes[i].is_ascii_digit())
        && DASHES.iter().all(|&i| bytes[i] == b'-')
        && COLONS.iter().all(|&i| bytes[i] == b':')
        && bytes[10] == b' '
}

/// One printable, bounded line. Control characters become spaces so a worker
/// cannot move the cursor around inside the panel's own text.
fn clamp(text: &str) -> String {
    let cleaned: String = text
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim_end().to_string();
    match cleaned.char_indices().nth(MAX_LINE_CHARS) {
        Some((idx, _)) => cleaned[..idx].to_string() + "...",
        None => cleaned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The tail is one process-wide reader, so these tests take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // `stop` deliberately keeps what it read; a test starts from nothing.
        *lock() = None;
        lines().clear();
        guard
    }

    fn scratch(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "hacash-panel-tail-{}-{name}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn append(path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("append to the log");
        file.write_all(text.as_bytes()).expect("write");
    }

    /// Poll without waiting out the interval: the tests are about the bytes.
    fn poll_now() -> Vec<Line> {
        read_immediately();
        poll()
    }

    #[test]
    fn a_previous_runs_log_is_never_replayed_as_todays_events() {
        let _serial = serial();
        let dir = scratch("replay");
        let path = dir.join("poworker.log");
        append(&path, "2026-07-28 09:00:00 [Mining] yesterday\n");

        start(&path);
        assert!(
            poll_now().is_empty(),
            "what was already in the file belongs to the previous run"
        );

        append(&path, "2026-07-29 14:03:11 [Mining] today\n");
        let lines = poll_now();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].stamp, "2026-07-29 14:03:11");
        assert_eq!(lines[0].text, "[Mining] today");

        stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_split_across_two_reads_arrives_once_and_whole() {
        let _serial = serial();
        let dir = scratch("partial");
        let path = dir.join("poworker.log");
        std::fs::write(&path, b"").expect("create");
        start(&path);

        append(&path, "2026-07-29 14:03:11 [Mining] half a l");
        assert!(poll_now().is_empty(), "a half line is not a line");
        append(&path, "ine\n");
        let lines = poll_now();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "[Mining] half a line");

        stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_is_followed_instead_of_ending_the_tail() {
        let _serial = serial();
        let dir = scratch("rotate");
        let path = dir.join("poworker.log");
        std::fs::write(&path, b"").expect("create");
        start(&path);
        append(&path, "2026-07-29 14:03:11 [Mining] before\n");
        assert_eq!(poll_now().len(), 1);

        // What the writer does: move the live file aside, start a new one.
        std::fs::rename(&path, dir.join("poworker.log.1")).expect("rotate");
        append(&path, "2026-07-29 14:04:00 [Mining] after\n");

        let lines = poll_now();
        assert_eq!(lines.len(), 1, "the new file must be picked up");
        assert_eq!(lines[0].text, "[Mining] after");

        stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_ring_is_bounded_however_much_the_worker_printed() {
        let _serial = serial();
        let dir = scratch("bounded");
        let path = dir.join("poworker.log");
        std::fs::write(&path, b"").expect("create");
        start(&path);

        for i in 0..(MAX_LINES * 3) {
            append(&path, &format!("2026-07-29 14:03:11 [Mining] batch {i}\n"));
        }
        // Several polls, because one poll reads a bounded slice.
        let mut last = Vec::new();
        for _ in 0..20 {
            let batch = poll_now();
            if batch.is_empty() {
                break;
            }
            last = batch;
        }

        let kept = recent(MAX_LINES * 3);
        assert_eq!(kept.len(), MAX_LINES, "the ring must stay bounded");
        assert_eq!(kept[0].text, "[Mining] batch 1499", "newest first");
        assert_eq!(
            last.last().map(|l| l.text.as_str()),
            Some("[Mining] batch 1499"),
            "and nothing at the end was skipped"
        );

        stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_without_a_timestamp_keeps_all_of_its_text() {
        assert_eq!(
            parse_line("2026-07-29 14:03:11 [Mining] stamped\n"),
            Line {
                stamp: "2026-07-29 14:03:11".to_string(),
                text: "[Mining] stamped".to_string(),
            }
        );
        // Nothing that merely looks close may cost a line its first 19 characters.
        for raw in [
            "[Mining] no stamp here at all",
            "2026-07-29T14:03:11 [Mining] wrong separator",
            "short",
        ] {
            let line = parse_line(raw);
            assert!(line.stamp.is_empty(), "{raw}");
            assert_eq!(line.text, raw);
        }
    }

    #[test]
    fn nothing_a_worker_prints_can_move_the_cursor_or_fill_the_ui() {
        let line = parse_line("2026-07-29 14:03:11 [Mining] a\u{1b}[2Jb\r\n");
        assert_eq!(line.text, "[Mining] a [2Jb");
        let long = parse_line(&format!("2026-07-29 14:03:11 {}", "x".repeat(9_000)));
        assert_eq!(long.text.chars().count(), MAX_LINE_CHARS + 3);
    }

    #[test]
    fn polling_with_no_worker_is_free_and_silent() {
        let _serial = serial();
        assert!(poll().is_empty());
        assert!(recent(10).is_empty());
        assert_eq!(line_count(), 0);
        assert_eq!(source_path(), None);
    }

    #[test]
    fn a_piped_worker_fills_the_same_ring_the_file_would() {
        // The Logs screen has one source. On the platforms where the worker is
        // piped rather than tailed, these are the lines it must show.
        let _serial = serial();
        record_piped("[Mining] from the pipe\n");
        record_piped("   ");
        record_piped("[Mining] second\r\n");

        let kept = recent(10);
        assert_eq!(kept.len(), 2, "a blank line is not a line");
        assert_eq!(kept[0].text, "[Mining] second");
        assert_eq!(
            kept[0].stamp, "",
            "a piped line carries no time of its own, and the panel invents none"
        );
        assert_eq!(line_count(), 2);
    }

    #[test]
    fn the_ring_is_bounded_the_same_way_whichever_source_filled_it() {
        let _serial = serial();
        for i in 0..(MAX_LINES + 25) {
            record_piped(&format!("[Mining] batch {i}"));
        }
        assert_eq!(line_count(), MAX_LINES);
        assert_eq!(recent(1)[0].text, format!("[Mining] batch {}", MAX_LINES + 24));
    }

    #[test]
    fn switching_worker_does_not_show_the_other_workers_log_under_its_name() {
        let _serial = serial();
        let dir = scratch("switch");
        let po = dir.join("poworker.log");
        let dia = dir.join("diaworker.log");
        std::fs::write(&po, b"").expect("create");
        std::fs::write(&dia, b"").expect("create");

        start(&po);
        record_piped("[Mining] hashing");
        assert_eq!(line_count(), 1);
        assert_eq!(source_path().as_deref(), Some(po.as_path()));

        start(&dia);
        assert_eq!(
            line_count(),
            0,
            "a different worker is a different log, not a continuation"
        );
        assert_eq!(source_path().as_deref(), Some(dia.as_path()));

        stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stopping_and_restarting_keeps_the_log_the_miner_is_reading() {
        let _serial = serial();
        let dir = scratch("restart");
        let path = dir.join("poworker.log");
        std::fs::write(&path, b"").expect("create");

        start(&path);
        append(&path, "2026-07-29 14:03:11 [Mining] first run\n");
        assert_eq!(poll_now().len(), 1);

        // Stopping the worker must not blank the log view: that is the moment
        // the miner most wants to read it.
        stop();
        assert!(poll_now().is_empty(), "a stopped tail looks for nothing");
        assert_eq!(recent(10).len(), 1, "and keeps what it already read");

        // An automatic restart continues the same log rather than wiping it.
        append(&path, "2026-07-29 14:04:00 [Mining] while stopped\n");
        start(&path);
        append(&path, "2026-07-29 14:05:00 [Mining] second run\n");
        let lines = poll_now();
        assert_eq!(lines.len(), 1, "only what the new run printed");
        assert_eq!(lines[0].text, "[Mining] second run");
        assert_eq!(recent(10).len(), 2, "the first run is still readable");

        stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
