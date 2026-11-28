//! In-game sessions from outside the process (app.md 4.1; design d3, test 4): the pe-app binary, headless, does `new`
//! 20 times and then a join it cancels, all through its console. Watched from outside: its OS thread count comes back
//! to what it was after the first session, no swap used D8's fallback port, no child process is left, and a typed
//! `quit` ends it with exit code 0.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// pe-app, headless, opening a session of its own on `port`, its output in `console.txt`.
struct Run {
    child: Child,
    dir: PathBuf,
}

impl Run {
    fn open(name: &str, port: u16) -> Run {
        Run::open_with(name, port, &[])
    }

    /// [`Run::open`] with these arguments too.
    fn open_with(name: &str, port: u16, extra: &[&str]) -> Run {
        let dir = std::env::temp_dir().join(format!("pe-app-sessions-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a test folder");
        let console = std::fs::File::create(dir.join("console.txt")).expect("console");
        let child = Command::new(env!("CARGO_BIN_EXE_pe-app"))
            .args(["--headless", "--open", "--listen", &format!("127.0.0.1:{port}"), "--identity"])
            .arg(dir.join("id.seed"))
            .args(extra)
            // No script builds: they'd need the toolchain.
            .env_remove("CARGO")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(console.try_clone().expect("console")))
            .stderr(Stdio::from(console))
            .spawn()
            .expect("pe-app starts");
        let run = Run { child, dir };
        run.wait_for("session 1: open", Duration::from_secs(120));
        run
    }

    fn console(&self) -> String {
        std::fs::read_to_string(self.dir.join("console.txt")).unwrap_or_default()
    }

    fn wait_for(&self, what: &str, within: Duration) {
        let end = Instant::now() + within;
        while !self.console().contains(what) {
            assert!(Instant::now() < end, "no {what:?} within {within:?}; console:\n{}", self.console());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn type_line(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().expect("piped");
        writeln!(stdin, "{line}").expect("typed");
        stdin.flush().expect("typed");
    }

    /// The first invite the console printed.
    fn invite(&self) -> String {
        let c = self.console();
        let line = c.lines().find_map(|l| l.strip_prefix("invite: ")).expect("an invite line");
        line.trim().to_string()
    }

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
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A UDP port nobody holds now.
fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// The process's OS threads, read from outside it.
fn threads(pid: u32) -> usize {
    #[cfg(windows)]
    {
        let out = Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!("(Get-Process -Id {pid}).Threads.Count")])
            .output()
            .expect("powershell");
        String::from_utf8_lossy(&out.stdout).trim().parse().expect("a thread count")
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir(format!("/proc/{pid}/task")).expect("/proc").count()
    }
    // macOS has no /proc: `ps -M` prints a header, then one line per thread.
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let out = Command::new("ps").args(["-M", "-p", &pid.to_string()]).output().expect("ps");
        assert!(out.status.success(), "ps -M: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).lines().skip(1).filter(|l| !l.trim().is_empty()).count()
    }
}

/// The process's child processes, read from outside it.
fn children(pid: u32) -> Vec<String> {
    #[cfg(windows)]
    {
        let q = format!(
            "Get-CimInstance Win32_Process -Filter 'ParentProcessId={pid}' | ForEach-Object {{ \"$($_.ProcessId) $($_.Name)\" }}"
        );
        let out = Command::new("powershell").args(["-NoProfile", "-Command", &q]).output().expect("powershell");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()
    }
    #[cfg(not(windows))]
    {
        let out = Command::new("pgrep").args(["-P", &pid.to_string()]).output().expect("pgrep");
        String::from_utf8_lossy(&out.stdout).lines().map(String::from).collect()
    }
}

/// The steady thread count: the lowest of a few readings a moment apart, since short-lived threads (a clock sample, a
/// rayon pool warming) come and go.
fn steady_threads(pid: u32) -> usize {
    std::thread::sleep(Duration::from_secs(2));
    (0..3)
        .map(|_| {
            std::thread::sleep(Duration::from_millis(300));
            threads(pid)
        })
        .min()
        .unwrap()
}

#[test]
fn twenty_new_sessions_and_a_cancelled_join_leave_no_thread_or_process_behind() {
    // An invite nobody will answer: A opens, prints it, and ends.
    let a = Run::open("a", free_port());
    let invite = a.invite();
    drop(a);

    let mut b = Run::open("b", free_port());
    let pid = b.child.id();
    let first = steady_threads(pid);
    for n in 2..=21 {
        b.type_line("new");
        b.wait_for(&format!("session {n}: open"), Duration::from_secs(60));
    }
    b.type_line(&format!("join {invite}"));
    b.wait_for("session 22: joining", Duration::from_secs(60));
    std::thread::sleep(Duration::from_millis(500));
    b.type_line("cancel");
    b.wait_for("session 23: open", Duration::from_secs(60));
    let last = steady_threads(pid);
    let left = children(pid);
    let console = b.console();
    eprintln!("threads after session 1: {first}, after session 23: {last}; children: {left:?}");
    assert!(last <= first, "threads left behind: {first} after session 1, {last} after 23\n{console}");
    assert!(left.is_empty(), "child processes left: {left:?}");
    assert!(!console.contains("was busy"), "a swap used D8's fallback port\n{console}");
    b.type_line("quit");
    assert_eq!(b.wait_exit(Duration::from_secs(60)), Some(0), "{}", b.console());
}

/// The test fault belongs to session 1 (app.md 4.1, D5): after `new`, the console says it ended, a typed `fault`
/// is refused as ended, and session 2 runs without it.
#[test]
fn an_injected_fault_ends_with_session_1() {
    let mut b = Run::open_with("fault", free_port(), &["--inject-fault-at", "1000000"]);
    b.type_line("new");
    b.wait_for("session 2: open", Duration::from_secs(60));
    b.wait_for("the test fault ended with session 1", Duration::from_secs(5));
    b.type_line("fault");
    b.wait_for("`fault` is refused: the test fault ended with session 1", Duration::from_secs(10));
    b.type_line("quit");
    assert_eq!(b.wait_exit(Duration::from_secs(60)), Some(0), "{}", b.console());
}
