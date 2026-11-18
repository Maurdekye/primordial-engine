//! `--log` against each way a run ends (@coordinator-opus mc76d9fd9b119): a typed `quit`, a second request, Ctrl+C,
//! a close and a panic. In each, the log's last line, its time prefix taken off, equals the console's last line. Here
//! the console is one file that both of the child's output streams append to, in the order pe-app wrote them.
//!
//! Ctrl+C is SIGINT on unix and, on Windows, Ctrl+Break to the child's own process group (console.rs handles both as
//! Ctrl+C), sent only when this test has a console to share; a close is SIGHUP on unix. Windows' own Ctrl+C, a closed
//! console window and the PowerShell 5.1 command line are checked by hand on a real console (the M1 dry run's
//! evidence). The panic is the console's `panic`, which only debug builds take.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// pe-app, headless and alone, with `--log`, its output in `console.txt`.
struct Run {
    child: Child,
    dir: PathBuf,
}

impl Run {
    fn start(name: &str) -> Run {
        Run::start_with(name, &[])
    }

    /// [`Run::start`] with these environment variables set.
    fn start_with(name: &str, env: &[(&str, &str)]) -> Run {
        let dir = std::env::temp_dir().join(format!("pe-app-console-log-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a test folder");
        let console = OpenOptions::new().create(true).append(true).open(dir.join("console.txt")).expect("console");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pe-app"));
        cmd.arg("--headless")
            .arg("--identity")
            .arg(dir.join("id.seed"))
            .arg("--log")
            .arg(dir.join("run.log"))
            // No script builds (they'd need the toolchain) and the panic's short form.
            .env_remove("CARGO")
            .env_remove("RUST_BACKTRACE")
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::from(console.try_clone().expect("console")))
            .stderr(Stdio::from(console));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
        let run = Run { child: cmd.spawn().expect("pe-app starts"), dir };
        run.wait_for("headless: running", Duration::from_secs(120));
        run
    }

    fn console(&self) -> String {
        std::fs::read_to_string(self.dir.join("console.txt")).unwrap_or_default()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("run.log")).unwrap_or_default()
    }

    fn wait_for(&self, what: &str, within: Duration) {
        let end = Instant::now() + within;
        while !self.console().contains(what) {
            assert!(Instant::now() < end, "no {what:?} within {within:?}; console:\n{}", self.console());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn type_lines(&mut self, text: &str) {
        let stdin = self.child.stdin.as_mut().expect("piped");
        stdin.write_all(text.as_bytes()).expect("typed");
        stdin.flush().expect("typed");
    }

    /// The exit code, once the process has ended (`None` when a signal ended it).
    fn wait_exit(&mut self, within: Duration) -> Option<i32> {
        let end = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status.code();
            }
            assert!(Instant::now() < end, "still running after {within:?}; console:\n{}", self.console());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The console's last line and the log's, its time prefix (`{seconds:9.3} `) taken off.
    fn last_lines(&self) -> (String, String) {
        let console = self.console();
        let log = self.log();
        let c = console.lines().rev().find(|l| !l.is_empty()).unwrap_or_default().to_string();
        let l = log.lines().next_back().unwrap_or_default();
        let (secs, rest) = l.split_at(l.find(' ').map_or(0, |i| i + 1).max(10).min(l.len()));
        assert!(secs.trim().parse::<f64>().is_ok(), "a log line without its time: {l:?}");
        (c, rest.to_string())
    }

    fn assert_same_last_line(&self) {
        let (console, log) = self.last_lines();
        assert_eq!(
            log,
            console,
            "the log's last line is the console's\nconsole:\n{}\nlog:\n{}",
            self.console(),
            self.log()
        );
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_typed_quit_ends_the_run_and_the_log_keeps_its_end_lines() {
    let mut run = Run::start("quit");
    run.type_lines("quit\n");
    assert_eq!(run.wait_exit(Duration::from_secs(60)), Some(0), "{}", run.console());
    run.assert_same_last_line();
    let log = run.log();
    assert!(log.contains("pe-app: quitting (typed `quit`)"), "{log}");
    assert!(log.lines().next_back().unwrap_or_default().contains("simulation: SimStats"), "the end line last:\n{log}");
}

#[test]
fn a_second_request_ends_the_run_at_once_and_the_log_says_so() {
    let mut run = Run::start("twice");
    // The second `quit` is the second Ctrl+C: it ends the process at once, cutting the quit short.
    run.type_lines("quit\nquit\n");
    assert_eq!(run.wait_exit(Duration::from_secs(60)), Some(130), "{}", run.console());
    run.assert_same_last_line();
    assert!(run.log().contains("pe-app: ended at once"), "{}", run.log());
}

#[test]
fn a_panic_is_in_the_log_as_on_the_console() {
    if !cfg!(debug_assertions) {
        eprintln!("skipped: only debug builds take the console's `panic`");
        return;
    }
    let mut run = Run::start("panic");
    run.type_lines("panic\n");
    run.wait_for("RUST_BACKTRACE", Duration::from_secs(30));
    // The console's thread is gone; end the process as a kill would, then compare.
    let _ = run.child.kill();
    let _ = run.child.wait();
    run.assert_same_last_line();
    let log = run.log();
    assert!(log.contains("panicked at") && log.contains("the console's test panic"), "{log}");
}

#[test]
fn a_panic_has_its_backtrace_when_rust_backtrace_asks_as_the_standard_hook_decides() {
    if !cfg!(debug_assertions) {
        eprintln!("skipped: only debug builds take the console's `panic`");
        return;
    }
    // RUST_LIB_BACKTRACE is for `Backtrace::capture` in programs; the standard hook reads RUST_BACKTRACE only.
    let mut run = Run::start_with("backtrace", &[("RUST_BACKTRACE", "1"), ("RUST_LIB_BACKTRACE", "0")]);
    run.type_lines("panic\n");
    run.wait_for("the console's test panic", Duration::from_secs(30));
    let _ = run.child.kill();
    let _ = run.child.wait();
    run.assert_same_last_line();
    let log = run.log();
    assert!(log.contains("stack backtrace:") && !log.contains("note: run with"), "{log}");
}

#[test]
fn a_log_that_exists_is_refused_before_anything_runs() {
    let dir = std::env::temp_dir().join(format!("pe-app-console-log-{}-exists", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a test folder");
    let log = dir.join("run.log");
    std::fs::write(&log, "an earlier run\n").expect("write");
    let out = Command::new(env!("CARGO_BIN_EXE_pe-app"))
        .args(["--headless", "--seconds", "30", "--log"])
        .arg(&log)
        .env_remove("CARGO")
        .stdin(Stdio::null())
        .output()
        .expect("pe-app runs");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("never overwrites a log"), "{err}");
    assert_eq!(std::fs::read_to_string(&log).expect("read"), "an earlier run\n", "untouched");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
mod signals {
    use super::*;

    fn signal(run: &Run, sig: i32) {
        // SAFETY: a plain kill(2) on our own child's pid.
        assert_eq!(unsafe { libc::kill(run.child.id() as i32, sig) }, 0, "kill");
    }

    #[test]
    fn ctrl_c_quits_gracefully_and_the_log_keeps_the_end_lines() {
        let mut run = Run::start("sigint");
        signal(&run, libc::SIGINT);
        assert_eq!(run.wait_exit(Duration::from_secs(60)), Some(0), "{}", run.console());
        run.assert_same_last_line();
        assert!(run.log().contains("pe-app: quitting (asked from the console)"), "{}", run.log());
    }

    #[test]
    fn a_second_ctrl_c_ends_at_once() {
        let mut run = Run::start("sigint2");
        signal(&run, libc::SIGINT);
        signal(&run, libc::SIGINT);
        let code = run.wait_exit(Duration::from_secs(60));
        run.assert_same_last_line();
        // The second signal usually lands while the quit runs (130); if the quit won the race, it ended with 0.
        assert!(matches!(code, Some(0) | Some(130)), "{code:?}\n{}", run.console());
    }

    #[test]
    fn a_close_quits_and_the_log_keeps_the_end_lines() {
        let mut run = Run::start("sighup");
        signal(&run, libc::SIGHUP);
        assert_eq!(run.wait_exit(Duration::from_secs(60)), Some(0), "{}", run.console());
        run.assert_same_last_line();
    }
}

#[cfg(windows)]
mod ctrl_break {
    use super::*;

    const CTRL_BREAK_EVENT: u32 = 1;

    unsafe extern "system" {
        fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
        fn GetConsoleCP() -> u32;
    }

    #[test]
    fn ctrl_c_quits_gracefully_and_the_log_keeps_the_end_lines() {
        // SAFETY: GetConsoleCP takes nothing and only reads this process's console.
        if unsafe { GetConsoleCP() } == 0 {
            eprintln!("skipped: this test has no console to share with pe-app, so no Ctrl+Break can reach it");
            return;
        }
        let mut run = Run::start("break");
        // SAFETY: the child is in its own process group (CREATE_NEW_PROCESS_GROUP), whose id is its pid.
        assert_ne!(unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, run.child.id()) }, 0, "Ctrl+Break sent");
        assert_eq!(run.wait_exit(Duration::from_secs(60)), Some(0), "{}", run.console());
        run.assert_same_last_line();
        assert!(run.log().contains("pe-app: quitting (asked from the console)"), "{}", run.log());
    }
}
