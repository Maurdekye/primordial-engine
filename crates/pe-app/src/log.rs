//! The run's log, `--log <file>` (@coordinator-opus's ruling mc76d9fd9b119, after M1's dry run): every line pe-app
//! prints goes to the console as before and, with `--log`, also to a UTF-8 file, each line prefixed with the seconds
//! since the log opened. The file is opened once, before the session starts, and never overwrites a file that exists.
//!
//! **Why pe-app writes it itself.** Windows PowerShell 5.1 ends a program piped to `Tee-Object` (or redirected with
//! `2>`) at once on Ctrl+C, before its handler runs, so the graceful leave never happens and the end lines are lost
//! (the dry run, [V]); it also writes those logs in UTF-16 and garbles non-ASCII text. With `--log` the command needs
//! no pipe or redirection: Ctrl+C reaches pe-app, and the console shows the text as written.
//!
//! **Every exit path keeps the last line.** Each line is written through: [`write_str`] writes the file, then the
//! console, under one lock, with nothing buffered in the process. So whatever ends the process, `std::process::exit`
//! (console.rs, `fail`), a panic, or Windows ending it after a close event, the file already holds every line the
//! console showed, in the same order. A panic prints through here too ([`install_panic_hook`]), in place of the
//! standard hook's message, so it lands in both. pe-xr's and pe-mcp's own lines and Box3D's warning come through
//! their sinks (`pe_xr::diag::set_sink`, `pe_mcp::host::set_line_sink`, `pe_physics::set_warning_sink`), which
//! pe-app points at [`err_line`].
//!
//! **A file that can't be written stops, and says so.** If a write to the file fails (a full disk, or a drive that
//! went away), the console says once that the log stops there, with the error, and the log closes: the lines after
//! it reach the console only, and a reader of the file isn't left trusting one that fell behind (@design-reviewer's
//! L1, m524453d20b1f).
//!
//! **What the file can't hold.** Text that native code writes straight to the process's standard error (a GPU
//! driver's or the OpenXR loader's own messages), wasmtime's own messages before it aborts (a fault at an address
//! no linear memory knows, or a trap handler it can't remove) and the Rust runtime's last words before an abort (a
//! stack overflow) reach the console only: none of them passes through pe-app.
//!
//! Without `--log` the macros print exactly as `println!` and `eprintln!` did, except that a console write that fails
//! is ignored instead of panicking: a console closed during the quit must not cut the leave short.
//!
//! The `out!` and `err!` macros, at the crate's root, are `println!` and `eprintln!` through here.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Which console stream a line goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

/// A log file: the lines written to it, each prefixed with the seconds since it opened.
#[derive(Debug)]
pub struct LogFile {
    file: File,
    path: PathBuf,
    opened: Instant,
}

impl LogFile {
    /// Creates the file at `path`. Refuses one that exists (`AlreadyExists`): pe-app never overwrites a log. The first
    /// line says when it opened, as Unix time, so two machines' logs line up.
    pub fn create(path: &Path) -> std::io::Result<LogFile> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        writeln!(file, "# pe-app log, opened at Unix time {unix:.3}; each line starts with the seconds since then")?;
        Ok(LogFile { file, path: path.to_path_buf(), opened: Instant::now() })
    }

    /// Writes `text`, each of its lines prefixed with the seconds since the log opened. Errors are returned, not
    /// printed: the caller is the printing path itself.
    pub fn write_text(&mut self, text: &str) -> std::io::Result<()> {
        let at = self.opened.elapsed().as_secs_f64();
        let mut buf = String::with_capacity(text.len() + 16);
        for line in text.split('\n') {
            buf.push_str(&format!("{at:9.3} {line}\n"));
        }
        self.file.write_all(buf.as_bytes())
    }
}

/// The process's log, once `--log` opened it.
static LOG: Mutex<Option<LogFile>> = Mutex::new(None);

fn lock() -> std::sync::MutexGuard<'static, Option<LogFile>> {
    // A panic while it was held can't leave it half-written in a way that matters more than losing the log.
    LOG.lock().unwrap_or_else(|e| e.into_inner())
}

/// Opens the process's log at `path` (`--log`), before anything else prints that should be in it. Refuses a file that
/// exists. Once per process: a second call is refused.
pub fn open(path: &Path) -> std::io::Result<()> {
    let mut g = lock();
    if g.is_some() {
        return Err(std::io::Error::other("the log is already open"));
    }
    *g = Some(LogFile::create(path)?);
    Ok(())
}

/// Whether `--log` opened a log.
pub fn is_open() -> bool {
    lock().is_some()
}

/// Prints `text` on `stream` as `println!` or `eprintln!` would, and writes it to the log if one is open: the file
/// first, then the console, under one lock, so both hold the same lines in the same order and the file is never
/// behind (module docs). A failed console write is ignored; a failed file write closes the log, and the console says
/// so once ([`write_log`]).
pub fn write_str(stream: Stream, text: &str) {
    // Nothing in this locked section may panic: `LOG` isn't re-entrant, so the panic hook's own `write_str` would wait
    // for this lock forever, on this same thread (@design-reviewer, m524453d20b1f).
    let mut g = lock();
    if let Some(stopped) = write_log(&mut g, text) {
        let _ = writeln!(std::io::stderr().lock(), "{stopped}");
    }
    match stream {
        Stream::Out => {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{text}");
            let _ = out.flush();
        }
        Stream::Err => {
            let _ = writeln!(std::io::stderr().lock(), "{text}");
        }
    }
}

/// Writes `text` to the log in `slot`, if one is open. If the write fails, closes the log, so it isn't tried again on
/// every line, and returns the line that tells the console the file stops here (module docs).
fn write_log(slot: &mut Option<LogFile>, text: &str) -> Option<String> {
    let e = slot.as_mut()?.write_text(text).err()?;
    let path = slot.take()?.path;
    Some(format!(
        "pe-app: the log {} stops here, since writing it failed ({e}); the lines from here on are on this console only",
        path.display()
    ))
}

/// [`write_str`] on standard error: the sink pe-app gives pe-xr and pe-mcp for their lines.
pub fn err_line(text: &str) {
    write_str(Stream::Err, text);
}

/// Prints a panic through [`write_str`], in place of the standard hook's message, so the log ends with it as the
/// console does (module docs). Installed with `--log` only; without it the standard hook stays.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "Box<dyn Any>".into());
        let at = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())).unwrap_or_default();
        // Decided as the standard hook decides: by RUST_BACKTRACE alone (`full` for every frame; unset or `0` for
        // none, with the note on the first panic only). `Backtrace::capture` would read RUST_LIB_BACKTRACE first
        // (@design-reviewer's N2, m524453d20b1f).
        let backtrace = |full: bool| {
            let b = std::backtrace::Backtrace::force_capture();
            let text = if full { format!("{b:#}") } else { format!("{b}") };
            // Its text ends with a newline, which would leave an empty last line.
            format!("\nstack backtrace:\n{}", text.trim_end())
        };
        let tail = match std::env::var_os("RUST_BACKTRACE") {
            Some(v) if v == "full" => backtrace(true),
            Some(v) if v != "0" => backtrace(false),
            _ if !NOTED.swap(true, Ordering::Relaxed) => {
                "\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace".to_string()
            }
            _ => String::new(),
        };
        write_str(Stream::Err, &format!("thread '{name}' panicked at {at}:\n{message}{tail}"));
    }));
}

/// Whether a panic has printed the note about RUST_BACKTRACE: the standard hook prints it once per process.
static NOTED: AtomicBool = AtomicBool::new(false);

/// `println!` through the run's log ([`write_str`]).
#[macro_export]
macro_rules! out {
    () => {
        $crate::log::write_str($crate::log::Stream::Out, "")
    };
    ($($arg:tt)*) => {
        $crate::log::write_str($crate::log::Stream::Out, &::std::format!($($arg)*))
    };
}

/// `eprintln!` through the run's log ([`write_str`]).
#[macro_export]
macro_rules! err {
    () => {
        $crate::log::write_str($crate::log::Stream::Err, "")
    };
    ($($arg:tt)*) => {
        $crate::log::write_str($crate::log::Stream::Err, &::std::format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("pe-app-log-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn a_log_never_overwrites_a_file_that_exists() {
        let p = temp("exists");
        std::fs::write(&p, "someone's file\n").expect("write");
        let e = LogFile::create(&p).expect_err("refused");
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&p).expect("read"), "someone's file\n", "untouched");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn each_line_is_prefixed_with_the_seconds_since_the_log_opened() {
        let p = temp("lines");
        let mut log = LogFile::create(&p).expect("create");
        log.write_text("one").expect("write");
        log.write_text("two\nthree \u{2026}").expect("write");
        drop(log);
        let text = std::fs::read_to_string(&p).expect("UTF-8");
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("# pe-app log, opened at Unix time "), "{}", lines[0]);
        assert_eq!(lines.len(), 4, "{text}");
        for (line, want) in lines[1..].iter().zip(["one", "two", "three \u{2026}"]) {
            let (secs, rest) = line.split_at(9);
            assert!(secs.trim().parse::<f64>().is_ok_and(|s| (0.0..60.0).contains(&s)), "{line}");
            assert_eq!(rest, format!(" {want}"));
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_file_that_cant_be_written_closes_the_log_and_the_console_is_told_once() {
        let p = temp("unwritable");
        let mut slot = Some(LogFile::create(&p).expect("create"));
        assert_eq!(write_log(&mut slot, "written"), None, "a write that works says nothing");
        assert!(slot.is_some(), "and the log stays open");
        // The same file opened read-only: every write to it fails, as on a full disk or a drive that went away.
        let file = File::open(&p).expect("open");
        slot = Some(LogFile { file, path: p.clone(), opened: Instant::now() });
        let said = write_log(&mut slot, "lost").expect("the console is told");
        assert!(
            said.starts_with(&format!("pe-app: the log {} stops here, since writing it failed (", p.display())),
            "{said}"
        );
        assert!(slot.is_none(), "the log closed");
        assert_eq!(write_log(&mut slot, "after"), None, "told once: the closed log isn't tried again");
        let text = std::fs::read_to_string(&p).expect("UTF-8");
        assert!(text.ends_with(" written\n"), "the file keeps what was written before:\n{text}");
        let _ = std::fs::remove_file(&p);
    }
}
