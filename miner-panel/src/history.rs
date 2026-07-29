//! The part of the performance chart that outlives one run of the panel.
//!
//! The worker publishes one snapshot at a time and remembers nothing, so every
//! curve on the dashboard is accumulated on this side. `dashboard::Series` does
//! that for as long as the panel is open; this file is the small amount of it
//! that is written down, so the 6H and 24H ranges are not empty every time the
//! panel is restarted.
//!
//! Three rules hold it together.
//!
//! 1. Nothing is invented. Every line in the file is a reading the worker
//!    actually published, with the worker's own timestamp on it. Restoring the
//!    file adds no points between them: a gap in the file stays a gap on the
//!    chart, which is what an hour with the panel closed really looks like.
//! 2. The file is bounded, twice. It holds at most one reading every half
//!    minute over one day, and it is refused outright if it has somehow grown
//!    past a size a day of readings could ever occupy.
//! 3. Anything unreadable is dropped, quietly and per line. This is a
//!    convenience cache next to the worker's own files; a corrupt one must cost
//!    an empty chart, never a panel that will not start.

use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::dashboard::{Sample, Series};

/// Two readings a minute is the resolution that survives a restart. The live
/// series is six times finer; this is what a day of it costs to keep.
///
/// It has to be this fine, and the reason is the chart rather than the eye.
/// `Series::segments` breaks the line at any hole longer than `dashboard::GAP_MS`
/// (one minute) and `area_chart` will not draw a run with one point in it, so a
/// grid of a whole minute puts every restored reading right on that boundary:
/// the readings really kept are the last one in each bucket, they drift by a few
/// seconds either way, and about half the intervals land just over the line.
/// The result was a day of measurements arriving as hundreds of single points
/// and being dropped, with Average and Peak filled in above an empty plot. At
/// half a minute the kept readings are always nearer together than an outage is,
/// so a restored run draws as the run it was and a real hole still reads as one.
const GRID_MS: u64 = 30_000;
/// One day at that grid, and the hard ceiling on the number of lines written or
/// read back.
const MAX_RECORDS: usize = 2 * 24 * 60;
/// The longest history the chart can show, so nothing older is worth keeping.
const SPAN_MS: u64 = 24 * 60 * 60 * 1_000;
/// A record is well under 30 bytes, so a full day is around 70 KB. 160 KB is
/// more than twice that, so a file past it did not come from this panel and is
/// not read.
const MAX_FILE_BYTES: u64 = 160 * 1024;
/// How often the file is rewritten. It is rewritten whole, and it is tiny.
const WRITE_INTERVAL_MS: u64 = 60_000;
/// A record dated in the future came from a clock that has since moved back,
/// not from a measurement. It is dropped on sight, and not only because it
/// would sit off the end of the axis: the live series takes readings in order,
/// so restoring a future timestamp would refuse every genuine reading the
/// session is about to take.

/// The file, beside the worker's own `miner-stats.json`.
pub fn path_beside_stats(stats_path: &Path) -> PathBuf {
    stats_path.with_file_name("miner-history.csv")
}

static RESTORED: Once = Once::new();
static NEXT_WRITE_MS: AtomicU64 = AtomicU64::new(0);

/// Load an earlier session's readings into the live series, once per process.
///
/// It must run before the first live reading is recorded: `Series` accepts
/// samples in order only, so anything restored after the session has started
/// would be refused as old news.
pub fn restore_once(path: &Path, series: &mut Series, now_ms: u64) {
    RESTORED.call_once(|| {
        for sample in read_file(path, now_ms) {
            series.record(sample.unix_ms, sample.hps as f64);
        }
    });
}

/// Write the history down, at most once per interval. Returns whether it wrote.
pub fn persist_if_due(path: &Path, series: &Series, now_ms: u64) -> bool {
    persist_on_clock(&NEXT_WRITE_MS, path, series, now_ms)
}

/// The same, against a caller-owned clock so the interval can be tested without
/// one test's writes deciding what another test sees.
fn persist_on_clock(next_write: &AtomicU64, path: &Path, series: &Series, now_ms: u64) -> bool {
    if now_ms < next_write.load(Relaxed) {
        return false;
    }
    next_write.store(now_ms.saturating_add(WRITE_INTERVAL_MS), Relaxed);
    let records = thin(&series.window(now_ms, SPAN_MS));
    if records.is_empty() {
        // Nothing measured yet. An empty file would only erase what an earlier
        // session did measure.
        return false;
    }
    write_file(path, &records).is_ok()
}

/// One reading per grid step, newest kept, bounded to a day of them.
fn thin(samples: &[Sample]) -> Vec<Sample> {
    let mut out: Vec<Sample> = Vec::new();
    for sample in samples {
        let bucket = sample.unix_ms / GRID_MS;
        match out.last_mut() {
            Some(last) if last.unix_ms / GRID_MS == bucket => *last = *sample,
            _ => out.push(*sample),
        }
    }
    if out.len() > MAX_RECORDS {
        out.drain(..out.len() - MAX_RECORDS);
    }
    out
}

fn render(records: &[Sample]) -> String {
    let mut text = String::with_capacity(records.len() * 24);
    for record in records {
        text.push_str(&format!("{},{}\n", record.unix_ms, record.hps));
    }
    text
}

/// Parse a history file. Every line is checked on its own: one unreadable line
/// costs that line and nothing else.
fn parse(text: &str, now_ms: u64) -> Vec<Sample> {
    let oldest = now_ms.saturating_sub(SPAN_MS);
    let newest = now_ms;
    let mut out: Vec<Sample> = Vec::new();
    for line in text.lines() {
        let Some((left, right)) = line.trim().split_once(',') else {
            continue;
        };
        let (Ok(unix_ms), Ok(hps)) = (left.trim().parse::<u64>(), right.trim().parse::<f32>())
        else {
            continue;
        };
        if unix_ms < oldest || unix_ms > newest || !hps.is_finite() || hps < 0.0 {
            continue;
        }
        out.push(Sample { unix_ms, hps });
    }
    // A file written by an interrupted process can hold its records out of
    // order; the series takes them in order or not at all.
    out.sort_by_key(|sample| sample.unix_ms);
    out.dedup_by_key(|sample| sample.unix_ms);
    if out.len() > MAX_RECORDS {
        out.drain(..out.len() - MAX_RECORDS);
    }
    out
}

fn read_file(path: &Path, now_ms: u64) -> Vec<Sample> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_FILE_BYTES => {}
        // Missing, unreadable, not a regular file, or larger than a day of
        // readings can be: there is nothing here this panel wrote.
        _ => return Vec::new(),
    }
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text, now_ms),
        Err(_) => Vec::new(),
    }
}

/// Replace the file in one step, so a panel that is killed mid-write leaves the
/// previous history intact rather than half of a new one.
fn write_file(path: &Path, records: &[Sample]) -> std::io::Result<()> {
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    // Every way out of here that is not the rename removes the temporary. A
    // failed write leaves a partial file behind exactly as a failed rename
    // does, and this runs once a minute for as long as the panel is open.
    if let Err(error) = std::fs::write(&temp, render(records).as_bytes()) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(now: u64, count: u64, step_ms: u64) -> Vec<Sample> {
        (0..count)
            .map(|i| Sample {
                unix_ms: now - (count - 1 - i) * step_ms,
                hps: 4.0e9 + i as f32,
            })
            .collect()
    }

    #[test]
    fn a_day_of_readings_is_written_at_one_a_grid_step_and_no_more() {
        // Two days of five-second samples is what a long-running panel really
        // holds. What reaches the file is one day of it, one reading per step.
        let now = 1_800_000_000_000u64;
        let dense = samples(now, 40_000, 5_000);
        let thinned = thin(&dense);
        assert!(thinned.len() <= MAX_RECORDS, "{}", thinned.len());
        for pair in thinned.windows(2) {
            assert!(
                pair[1].unix_ms / GRID_MS > pair[0].unix_ms / GRID_MS,
                "at most one reading per grid step survives"
            );
        }
        assert_eq!(
            thinned.last().unwrap().unix_ms,
            dense.last().unwrap().unix_ms,
            "the newest reading is never the one dropped"
        );
        assert!(
            (render(&thinned).len() as u64) < MAX_FILE_BYTES,
            "a full day must fit inside the size the reader accepts"
        );
    }

    #[test]
    fn what_is_written_is_what_comes_back() {
        let now = 1_800_000_000_000u64;
        let written = thin(&samples(now, 600, 5_000));
        let read = parse(&render(&written), now);
        assert_eq!(read, written);
    }

    #[test]
    fn a_restored_hour_draws_as_an_hour_and_not_as_dust() {
        // What a real session leaves behind: dense readings, thinned to the
        // grid, read back into the series the chart draws from. The chart
        // splits at every hole longer than a minute and skips any run with a
        // single point in it, so a grid coarse enough to look like a hole to
        // that rule would put the whole restored history on the floor: the
        // Average and Peak boxes would fill and the plot above them would be
        // empty. Two hours of readings is one run.
        //
        // The spacing is 5s plus a drift, which is what the readings really
        // look like: the panel re-reads the stats file every 350ms and keeps a
        // sample once five seconds have passed, so the interval is a little over
        // five seconds and never the same twice. Exactly 5,000ms would divide
        // the grid evenly and hide the whole problem.
        let now = 1_800_000_000_000u64;
        let mut at = now - 2 * 60 * 60 * 1_000;
        let mut dense: Vec<Sample> = Vec::new();
        while at <= now {
            dense.push(Sample {
                unix_ms: at,
                hps: 4.0e9,
            });
            at += 5_000 + (dense.len() as u64 * 37) % 350;
        }
        let mut series = Series::default();
        for sample in thin(&dense) {
            series.record(sample.unix_ms, sample.hps as f64);
        }
        let runs = series.segments(now, SPAN_MS);
        assert_eq!(
            runs.len(),
            1,
            "an unbroken two hours came back as {} pieces",
            runs.len()
        );
        assert!(runs[0].len() > 2, "and it has to be drawable");
    }

    #[test]
    fn a_restored_gap_stays_a_gap() {
        // The panel was closed for six hours. Restoring must not join the two
        // ends: it did not measure anything in between and must not imply it.
        let now = 1_800_000_000_000u64;
        let mut text = String::new();
        for i in 0..10u64 {
            text.push_str(&format!("{},{}\n", now - 23 * 3_600_000 + i * GRID_MS, 1.0e9));
        }
        for i in 0..10u64 {
            text.push_str(&format!("{},{}\n", now - 3_600_000 + i * GRID_MS, 2.0e9));
        }
        let mut series = Series::default();
        for sample in parse(&text, now) {
            series.record(sample.unix_ms, sample.hps as f64);
        }
        let runs = series.segments(now, SPAN_MS);
        assert_eq!(runs.len(), 2, "one outage, two runs");
        assert_eq!(runs[0].len(), 10);
        assert_eq!(runs[1].len(), 10);
    }

    #[test]
    fn a_damaged_file_costs_the_damaged_lines_and_nothing_else() {
        let now = 1_800_000_000_000u64;
        let text = format!(
            "{a},4000000000\nnonsense\n\n{b},not-a-number\n,\n{c},NaN\n{d},-5\n{e},4100000000\n",
            a = now - 120_000,
            b = now - 110_000,
            c = now - 100_000,
            d = now - 90_000,
            e = now - 60_000,
        );
        let read = parse(&text, now);
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].unix_ms, now - 120_000);
        assert_eq!(read[1].hps, 4.1e9);
    }

    #[test]
    fn readings_from_another_day_or_another_clock_are_not_charted() {
        let now = 1_800_000_000_000u64;
        let text = format!(
            "{old},1000\n{ahead},2000\n{ok},3000\n",
            old = now - SPAN_MS - 1,
            ahead = now + 1,
            ok = now - 1_000,
        );
        let read = parse(&text, now);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].hps, 3000.0);
    }

    #[test]
    fn a_future_dated_file_cannot_freeze_this_sessions_chart() {
        // The clock moved back between sessions. If the restored history kept
        // its future timestamps, `Series` would refuse every reading taken now,
        // and the chart would sit frozen for as long as the gap.
        let now = 1_800_000_000_000u64;
        let text = format!("{},4000000000\n", now + 3_600_000);
        let mut series = Series::default();
        for sample in parse(&text, now) {
            series.record(sample.unix_ms, sample.hps as f64);
        }
        assert!(series.record(now, 4.1e9), "today's reading must be kept");
        assert_eq!(series.latest().map(|s| s.unix_ms), Some(now));
    }

    #[test]
    fn a_file_bigger_than_a_day_of_readings_is_refused_whole() {
        let now = 1_800_000_000_000u64;
        let dir = std::env::temp_dir().join(format!("hacash-history-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("miner-history.csv");

        let good = thin(&samples(now, 120, GRID_MS));
        write_file(&path, &good).unwrap();
        assert_eq!(read_file(&path, now), good);
        // The same content, padded past the ceiling, is not read at all.
        let mut bloated = render(&good);
        while (bloated.len() as u64) <= MAX_FILE_BYTES {
            bloated.push_str(&format!("{},4000000000\n", now - 1));
        }
        std::fs::write(&path, bloated).unwrap();
        assert!(read_file(&path, now).is_empty());

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn writing_leaves_one_file_and_no_temporary() {
        let now = 1_800_000_000_000u64;
        let dir = std::env::temp_dir().join(format!("hacash-history-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("miner-history.csv");

        let mut series = Series::default();
        for sample in samples(now, 300, 5_000) {
            series.record(sample.unix_ms, sample.hps as f64);
        }
        let clock = AtomicU64::new(0);
        assert!(persist_on_clock(&clock, &path, &series, now));
        assert!(
            !persist_on_clock(&clock, &path, &series, now + 1_000),
            "the file is rewritten on its own interval, not every frame"
        );
        assert!(persist_on_clock(&clock, &path, &series, now + WRITE_INTERVAL_MS));

        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().map(Result::unwrap).collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path(), path);
        assert!(!read_file(&path, now).is_empty());

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn an_empty_session_never_erases_what_an_earlier_one_measured() {
        let now = 1_800_000_000_000u64;
        let dir = std::env::temp_dir().join(format!("hacash-history-keep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("miner-history.csv");

        let earlier = thin(&samples(now, 60, GRID_MS));
        write_file(&path, &earlier).unwrap();
        let clock = AtomicU64::new(0);
        assert!(!persist_on_clock(
            &clock,
            &path,
            &Series::default(),
            now + 10 * WRITE_INTERVAL_MS
        ));
        assert_eq!(read_file(&path, now), earlier);

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn the_history_sits_beside_the_workers_own_stats_file() {
        let path = path_beside_stats(Path::new("/miner/work/miner-stats.json"));
        assert_eq!(path.file_name().unwrap(), "miner-history.csv");
        assert_eq!(path.parent(), Path::new("/miner/work/miner-stats.json").parent());
    }
}
