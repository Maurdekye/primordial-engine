//! The cut log (`PE_CUT_LOG`; the pull-ahead plan's 1.2, for M1's two-machine acceptance: "the demo's script, with
//! both machines' roots logged at every cut"). Off unless the variable names a file. Then [`crate::sim::Sim`] holds a
//! [`CutLog`], and each final cut this machine finalizes writes one line. Off, a tick pays one branch on an empty
//! `Option`.
//!
//! The file starts with a `#` line, then has one `cut` line per finalize result, in the order the driver got them:
//!
//! ```text
//! cut t=<s> line=<16 hex> height=<n> frame=<n> root=<64 hex> result=agreed
//! cut t=<s> line=<16 hex> height=<n> frame=<n> root=<64 hex> result=diverged mine=<64 hex>
//! cut t=<s> line=<16 hex> height=<n> frame=<n> root=<64 hex> result=damaged
//! ```
//!
//! - `t` is host seconds since the log opened, as `--log`'s lines count. The `#` line gives the Unix time of the
//!   opening, so a line's Unix time is that plus `t`, and two machines' logs line up to within their clocks' offset.
//!   Lines match across machines by `line`, `height` and `frame`, not by `t`.
//! - `line` is the first 8 bytes of the cut's line, `root` the root the cut is final with (S3's `final_cut`), both in
//!   hex.
//! - `result` is this machine's [`Finalized`]: `agreed` when its own root at the cut is `root`; `diverged`, with its
//!   own root as `mine`; `damaged` when it holds no root of its own there. `NotReady` writes nothing: the driver asks
//!   again on a later tick. Otherwise the driver finalizes each final cut once, so a cut has at most one line.
//! - **The log holds the cuts this machine's driver finalized, not every final cut.** Each tick the driver finalizes
//!   only the newest final cut, so a cut that went final in the same tick as a newer one, or while an older one was
//!   still `NotReady`, gets no line here. That happens most when this machine catches up, after a stall or a slow
//!   stretch, and two machines then skip different heights. A height missing from one log means it wasn't judged on
//!   that machine, not a gap in agreement: compare the heights both logs carry, and count the rest as skipped.
//! - `diverged` and `damaged` need a cut final at a root this machine didn't vote (n >= 3), or its root there changing
//!   after its vote. At n = 2 every final cut needs both votes, so a split's cut goes final only after the repair has
//!   revoted the reference's root: it shows once, `agreed`, after a gap in `t`. The split itself is in `--log`
//!   (pe-diverge's `RootSplitStarted` and `RootSplitEnded`).
//!
//! The log flushes at least once a second of host time while the simulation runs, since [`CutLog::poll`] runs every
//! tick, even when no cut follows a line (as in a split), and when it is dropped. A write error is dropped: the log
//! never stops the simulation.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use pe_seams::{CutRef, Finalized, Hash, HostTime};

/// How often the log flushes, in host nanoseconds.
const FLUSH_NS: i64 = 1_000_000_000;

/// The cut log's sink and its clock.
pub struct CutLog {
    out: BufWriter<Box<dyn Write + Send>>,
    /// The host time the log opened at: `t` counts from it.
    opened: HostTime,
    /// When the log last flushed.
    flushed: Option<HostTime>,
    /// Whether a line was written since then.
    unflushed: bool,
    /// Lines written.
    lines: u64,
}

impl CutLog {
    /// The log `PE_CUT_LOG` names, if any, opened at host time `now`.
    pub fn from_env(now: HostTime) -> Option<std::io::Result<(CutLog, String)>> {
        let path = std::env::var_os("PE_CUT_LOG").filter(|p| !p.is_empty())?;
        let shown = Path::new(&path).display().to_string();
        Some(CutLog::create(Path::new(&path), now).map(|l| (l, shown)))
    }

    /// A new log in a new file at `path`, opened at host time `now` (an existing file is refused, as `--log` and
    /// `PE_TICK_TRACE` refuse one).
    pub fn create(path: &Path, now: HostTime) -> std::io::Result<CutLog> {
        let file: File = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
        CutLog::to_writer(Box::new(file), now)
    }

    /// A log written to `out` (a file, or a buffer in tests), opened at host time `now`.
    pub fn to_writer(out: Box<dyn Write + Send>, now: HostTime) -> std::io::Result<CutLog> {
        let mut out = BufWriter::new(out);
        let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        writeln!(
            out,
            "# pe-app cut log, opened at Unix time {:.3}: one `cut` line per final cut this machine finalized; t is the \
             seconds since then",
            unix.as_secs_f64()
        )?;
        out.flush()?;
        Ok(CutLog { out, opened: now, flushed: None, unflushed: false, lines: 0 })
    }

    /// Lines written so far.
    pub fn lines(&self) -> u64 {
        self.lines
    }

    /// One finalize result at `now`: `cut`, the `root` it is final with, and what this machine found there. `NotReady`
    /// writes nothing.
    pub fn record(&mut self, now: HostTime, cut: &CutRef, root: &Hash, result: &Finalized) {
        let _ = self.try_record(now, cut, root, result);
    }

    fn try_record(&mut self, now: HostTime, cut: &CutRef, root: &Hash, result: &Finalized) -> std::io::Result<()> {
        let tail = match result {
            Finalized::Agreed => "result=agreed".to_string(),
            Finalized::Diverged { mine } => format!("result=diverged mine={mine}"),
            Finalized::Damaged => "result=damaged".to_string(),
            Finalized::NotReady => return Ok(()),
        };
        let t = now.0.saturating_sub(self.opened.0) as f64 / 1e9;
        let line: String = cut.line.0.0[..8].iter().map(|b| format!("{b:02x}")).collect();
        writeln!(self.out, "cut t={t:.3} line={line} height={} frame={} root={root} {tail}", cut.height, cut.frame.0)?;
        self.lines += 1;
        self.unflushed = true;
        self.poll(now);
        Ok(())
    }

    /// Flushes the lines written since the last flush once a second of host time has passed since it (the first line
    /// at once). The simulation thread calls it every tick, so a line reaches the file within about a second even when
    /// no cut follows it.
    pub fn poll(&mut self, now: HostTime) {
        if self.unflushed && self.flushed.is_none_or(|f| now.0.saturating_sub(f.0) >= FLUSH_NS) {
            self.flushed = Some(now);
            self.flush();
        }
    }

    /// Writes out what is buffered.
    pub fn flush(&mut self) {
        self.unflushed = false;
        let _ = self.out.flush();
    }
}

impl Drop for CutLog {
    fn drop(&mut self) {
        self.flush();
    }
}

/// One `cut` line read back, for tests and checkers in Rust.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CutLine {
    pub line: String,
    pub height: u64,
    pub frame: u64,
    pub root: String,
    /// `agreed`, `diverged` or `damaged`.
    pub result: String,
    /// A `diverged` line's own root.
    pub mine: Option<String>,
}

/// The `cut` lines of a log's text; other lines are skipped.
pub fn parse(text: &str) -> Vec<CutLine> {
    text.lines()
        .filter(|l| l.starts_with("cut "))
        .filter_map(|l| {
            let field = |k: &str| {
                l.split_whitespace()
                    .find_map(|w| w.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
                    .map(str::to_string)
            };
            Some(CutLine {
                line: field("line")?,
                height: field("height")?.parse().ok()?,
                frame: field("frame")?.parse().ok()?,
                root: field("root")?,
                result: field("result")?,
                mine: field("mine"),
            })
        })
        .collect()
}

/// A writer that tests read back.
#[cfg(test)]
pub(crate) mod test_util {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub struct Shared(pub Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("the buffer").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        pub fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("the buffer").clone()).expect("utf-8")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::Shared;
    use super::*;
    use pe_seams::{Frame, LineId};

    fn cut(height: u64, frame: u64) -> CutRef {
        CutRef { line: LineId(Hash([0xab; 32])), height, frame: Frame(frame) }
    }

    #[test]
    fn each_result_writes_its_line_and_not_ready_writes_none() {
        let buf = Shared::default();
        // Opened a second before the first line: `t` counts from the opening.
        let mut log = CutLog::to_writer(Box::new(buf.clone()), HostTime(4_000_000_000)).expect("a log");
        let (root, mine) = (Hash([1; 32]), Hash([2; 32]));
        log.record(HostTime(5_000_000_000), &cut(1, 6), &root, &Finalized::Agreed);
        log.record(HostTime(5_100_000_000), &cut(2, 12), &root, &Finalized::NotReady);
        log.record(HostTime(5_250_000_000), &cut(2, 12), &root, &Finalized::Diverged { mine });
        log.record(HostTime(5_500_000_000), &cut(3, 18), &root, &Finalized::Damaged);
        log.flush();
        assert_eq!(log.lines(), 3);
        let text = buf.text();
        assert!(text.starts_with("# pe-app cut log, opened at Unix time "), "{text}");
        let r = "01".repeat(32);
        let lines: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(
            lines,
            [
                format!("cut t=1.000 line=abababababababab height=1 frame=6 root={r} result=agreed"),
                format!(
                    "cut t=1.250 line=abababababababab height=2 frame=12 root={r} result=diverged mine={}",
                    "02".repeat(32)
                ),
                format!("cut t=1.500 line=abababababababab height=3 frame=18 root={r} result=damaged"),
            ]
        );
        let parsed = parse(&text);
        assert_eq!(parsed.len(), 3);
        assert_eq!((parsed[1].height, parsed[1].frame, parsed[1].result.as_str()), (2, 12, "diverged"));
        assert_eq!(parsed[1].mine.as_deref(), Some("02".repeat(32).as_str()));
        assert_eq!(parsed[0].mine, None);
    }

    #[test]
    fn the_log_flushes_once_a_second_and_when_dropped() {
        let buf = Shared::default();
        let ms = |v: i64| HostTime(v * 1_000_000);
        let mut log = CutLog::to_writer(Box::new(buf.clone()), ms(0)).expect("a log");
        let root = Hash([1; 32]);
        // The first line flushes (nothing had), the next within the second doesn't, one a second later does.
        log.record(ms(0), &cut(1, 6), &root, &Finalized::Agreed);
        assert_eq!(parse(&buf.text()).len(), 1);
        log.record(ms(400), &cut(2, 12), &root, &Finalized::Agreed);
        assert_eq!(parse(&buf.text()).len(), 1, "buffered within the second");
        log.record(ms(1000), &cut(3, 18), &root, &Finalized::Agreed);
        assert_eq!(parse(&buf.text()).len(), 3);
        // With no cut after it (as in a split), a line still goes out a second after the last flush: the tick polls.
        log.record(ms(1100), &cut(4, 24), &root, &Finalized::Agreed);
        log.poll(ms(1900));
        assert_eq!(parse(&buf.text()).len(), 3, "buffered within the second");
        log.poll(ms(2000));
        assert_eq!(parse(&buf.text()).len(), 4, "the tick's poll flushed it");
        // A line after a quiet second goes out at once.
        log.record(ms(3500), &cut(5, 30), &root, &Finalized::Agreed);
        assert_eq!(parse(&buf.text()).len(), 5);
        log.record(ms(3600), &cut(6, 36), &root, &Finalized::Agreed);
        drop(log);
        assert_eq!(parse(&buf.text()).len(), 6, "dropped: flushed");
    }

    #[test]
    fn an_existing_file_is_refused() {
        // Beside the test executable, as the tick trace's test does, not in the system's temp folder.
        let exe = std::env::current_exe().expect("the test executable");
        let path = exe.parent().expect("its folder").join(format!("pe-cut-log-test-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let first = CutLog::create(&path, HostTime(0)).expect("a new file");
        drop(first);
        assert!(CutLog::create(&path, HostTime(0)).is_err(), "an existing file is refused");
        let _ = std::fs::remove_file(&path);
    }
}
