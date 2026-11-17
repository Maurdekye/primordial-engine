//! Quitting from the console (app.md 4): Ctrl+C, Ctrl+Break and closing the console window on Windows; SIGINT, SIGTERM
//! and SIGHUP on unix. The first request runs the same graceful quit as the end of a run, so a member in a shared
//! world leaves (its `Leave` goes out and it votes until the others confirm, or the quit's bound passes) instead of
//! vanishing and stalling the others' finality. A second request ends the process at once, which is also how a quit
//! already under way is cut short (`SimThread::leave_and_stop`).
//!
//! The shutdown runs exactly once, on whichever thread gets it first ([`Quit`] takes it out of its slot):
//! - before pe-app has a shutdown to run (setting up, or waiting for an invite's reply code), a request exits at once;
//! - while the desktop window runs, the request runs the shutdown (on a thread of its own for a close event), then
//!   exits: the window cannot be asked to close from outside;
//! - while VR runs, the main thread waits in short slices and runs the shutdown itself when it sees the request.
//!
//! Windows ends a process about 5 s after a close event, whatever its handler is doing. A graceful leave can take
//! longer (its bound follows the view interval), so a close waits at most [`CLOSE_WAIT`] for the quit, in either
//! mode, and then says "leave cut short": the others confirm the `Leave` if it went out, and otherwise see this
//! member drop, as after a crash.
//!
//! A log-off or a shut-down never reaches this handler: Windows sends those events only to services and ends an
//! interactive process first (Microsoft's HandlerRoutine page), so pe-app simply ends and the others see its
//! connection drop, as after a crash. Catching them would take the window's WM_QUERYENDSESSION (post-M1).
//!
//! On unix (tier 2), one thread takes the signals, and it handles them as Windows handles its events: SIGINT like
//! Ctrl+C, SIGTERM and SIGHUP like a close. So a second signal waits until a desktop quit that SIGINT started has
//! ended, and after SIGTERM or SIGHUP the "leave cut short" line can show although nothing ends the process there
//! and the quit goes on to finish.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The graceful quit, run once.
pub type Shutdown = Box<dyn FnOnce() + Send>;

/// How long a close event waits for the quit, inside Windows' 5 s.
pub const CLOSE_WAIT: Duration = Duration::from_millis(4500);

/// The exit code after a request that ended the process without the graceful quit (Ctrl+C's convention).
pub const EXIT_INTERRUPTED: i32 = 130;

enum State {
    /// No shutdown yet: a request exits at once.
    Early,
    /// The main thread polls [`Quit::requested`] and runs the shutdown itself.
    Polled,
    /// A request runs this shutdown on its own thread.
    Armed(Shutdown),
    /// The shutdown is running, or has run.
    Quitting,
}

/// What a request should do, decided under the lock ([`Quit::request`]).
pub enum Action {
    /// End the process at once with [`EXIT_INTERRUPTED`].
    ExitNow,
    /// Run this shutdown, then end the process with 0: no one else will run it.
    Run(Shutdown),
    /// The main thread will run the shutdown: return (a close event first waits for it, [`Quit::wait_quit`]).
    MainQuits,
}

impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Action::ExitNow => "ExitNow",
            Action::Run(_) => "Run",
            Action::MainQuits => "MainQuits",
        })
    }
}

/// How long the main thread waits for a request's quit, counted from the window's close, before ending the process
/// itself with [`EXIT_INTERRUPTED`]. It covers the leave's longest bound (15 s, app.md 4), and a shutdown that
/// panicked on the request's thread then can't hang pe-app. Before the leave, the quit also stops MCP and the builds,
/// and a build in progress can make that longer; closing the window during a quit is like a second request, so being
/// cut there is fine.
pub const REQUEST_QUIT_LIMIT: Duration = Duration::from_secs(20);

/// The console's quit requests and the shutdown they run (module docs).
pub struct Quit {
    state: Mutex<State>,
    requested: AtomicBool,
    quit_done: Mutex<bool>,
    quit_done_cv: Condvar,
}

impl Default for Quit {
    fn default() -> Self {
        Quit {
            state: Mutex::new(State::Early),
            requested: AtomicBool::new(false),
            quit_done: Mutex::new(false),
            quit_done_cv: Condvar::new(),
        }
    }
}

impl Quit {
    /// A quit that no console event reaches (tests): [`Self::request`] stands in for them.
    pub fn new() -> Arc<Quit> {
        Arc::new(Quit::default())
    }

    /// The process's quit, with the console's events hooked to it. Called first in `main`, before any thread starts:
    /// on unix it blocks the signals in every thread to come, and one thread of its own takes them. Once per process.
    pub fn install() -> Result<Arc<Quit>, String> {
        let cell = static_quit();
        if cell.get().is_some() {
            return Err("the console's quit is already installed".into());
        }
        let quit = cell.get_or_init(Quit::new).clone();
        os::hook()?;
        Ok(quit)
    }

    /// Whether a quit has been requested.
    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// From now on the main thread polls [`Self::requested`] and runs the shutdown itself (VR's wait).
    pub fn poll(&self) {
        let mut s = self.lock();
        if matches!(*s, State::Early | State::Armed(_)) {
            *s = State::Polled;
        }
    }

    /// From now on a request runs `shutdown` on its own thread. If a request already came, `shutdown` comes back for
    /// the caller to run now.
    pub fn arm(&self, shutdown: Shutdown) -> Option<Shutdown> {
        let mut s = self.lock();
        if self.requested() || matches!(*s, State::Quitting) {
            return Some(shutdown);
        }
        *s = State::Armed(shutdown);
        None
    }

    /// The main thread's own quit: the armed shutdown if no request has taken it, `None` if a request is running it
    /// (the process ends when it finishes). Either way no request runs it after this.
    pub fn take(&self) -> Option<Shutdown> {
        let mut s = self.lock();
        match std::mem::replace(&mut *s, State::Quitting) {
            State::Armed(f) => Some(f),
            _ => None,
        }
    }

    /// The main thread is quitting with what it holds, in polled mode or after [`Self::take`]: a later request ends
    /// the process at once.
    pub fn quitting(&self) {
        *self.lock() = State::Quitting;
    }

    /// The main thread's quit has finished: a close event waiting in [`Self::wait_quit`] may return.
    pub fn finished(&self) {
        *self.quit_done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.quit_done_cv.notify_all();
    }

    /// Waits up to `within` for [`Self::finished`]. False if it did not come.
    pub fn wait_quit(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        let mut done = self.quit_done.lock().unwrap_or_else(|e| e.into_inner());
        while !*done {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            done = self.quit_done_cv.wait_timeout(done, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }

    /// One request from the console. The first one quits gracefully: it takes an armed shutdown for the caller to run
    /// ([`Action::Run`]), or leaves it to the polling main thread ([`Action::MainQuits`]). A request before there is
    /// anything to quit, or any later request, answers [`Action::ExitNow`].
    pub fn request(&self) -> Action {
        if self.requested.swap(true, Ordering::AcqRel) {
            return Action::ExitNow;
        }
        let shutdown = {
            let mut s = self.lock();
            match std::mem::replace(&mut *s, State::Quitting) {
                State::Early => return Action::ExitNow,
                State::Polled => {
                    *s = State::Polled;
                    return Action::MainQuits;
                }
                State::Quitting => return Action::ExitNow,
                State::Armed(f) => f,
            }
        };
        Action::Run(shutdown)
    }

    /// A request from the operating system's handler: does what [`Self::request`] decides. `closing` is a close
    /// event, after which Windows ends the process anyway: the quit then runs on a thread of its own, and the handler
    /// waits for it at most [`CLOSE_WAIT`], in either mode (unix: SIGTERM or SIGHUP, module docs).
    fn handle(self: &Arc<Self>, closing: bool) {
        match self.request() {
            Action::ExitNow => {
                eprintln!("pe-app: ended at once");
                std::process::exit(EXIT_INTERRUPTED)
            }
            Action::Run(shutdown) => {
                eprintln!("pe-app: quitting (asked from the console)…");
                let q = self.clone();
                let run = move || {
                    shutdown();
                    q.finished();
                    std::process::exit(0)
                };
                if !closing {
                    // `run` ends the process.
                    run()
                } else {
                    let spawned = std::thread::Builder::new().name("pe-quit".into()).spawn(run);
                    if spawned.is_err() || !self.wait_quit(CLOSE_WAIT) {
                        eprintln!("pe-app: leave cut short (the console closed before the others confirmed it)");
                    }
                }
            }
            Action::MainQuits => {
                if closing && !self.wait_quit(CLOSE_WAIT) {
                    eprintln!("pe-app: leave cut short (the console closed before the others confirmed it)");
                }
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn installed() -> Option<&'static Arc<Quit>> {
    // `install` put it there; the handlers only run after it.
    static_quit().get()
}

fn static_quit() -> &'static OnceLock<Arc<Quit>> {
    static QUIT: OnceLock<Arc<Quit>> = OnceLock::new();
    &QUIT
}

#[cfg(windows)]
mod os {
    use super::installed;

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;

    type HandlerRoutine = unsafe extern "system" fn(ctrl_type: u32) -> i32;

    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<HandlerRoutine>, add: i32) -> i32;
    }

    /// Runs on a thread Windows starts for the event. Returning ends the process after a close. Log-off and shut-down
    /// reach only services (module docs); their arms are kept, as they cost nothing.
    unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
        let closing = match ctrl_type {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => false,
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => true,
            _ => return 0,
        };
        match installed() {
            Some(q) => {
                q.handle(closing);
                1
            }
            None => 0,
        }
    }

    pub fn hook() -> Result<(), String> {
        // SAFETY: `handler` is a valid HandlerRoutine for the life of the process.
        if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
            return Err(format!("SetConsoleCtrlHandler failed: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }
}

#[cfg(unix)]
mod os {
    use super::installed;

    /// Blocks SIGINT, SIGTERM and SIGHUP in this thread, so in every thread started after it (`install` runs first in
    /// `main`), and takes them on a thread of its own with `sigwait`. Child processes start with an empty mask
    /// (std's `Command`).
    pub fn hook() -> Result<(), String> {
        // SAFETY: plain libc calls on a local, initialised signal set.
        let set = unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                libc::sigaddset(&mut set, s);
            }
            let r = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            if r != 0 {
                return Err(format!("pthread_sigmask failed: {}", std::io::Error::from_raw_os_error(r)));
            }
            set
        };
        std::thread::Builder::new()
            .name("pe-console".into())
            .spawn(move || {
                loop {
                    let mut sig = 0;
                    // SAFETY: `set` is the initialised set blocked above.
                    if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                        continue;
                    }
                    if let Some(q) = installed() {
                        q.handle(sig != libc::SIGINT);
                    }
                }
            })
            .map(|_| ())
            .map_err(|e| format!("the console thread did not start: {e}"))
    }
}

#[cfg(not(any(windows, unix)))]
mod os {
    pub fn hook() -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn counting(n: &Arc<AtomicU32>) -> Shutdown {
        let n = n.clone();
        Box::new(move || {
            n.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[test]
    fn a_request_before_any_shutdown_exits_at_once() {
        let q = Quit::new();
        assert!(matches!(q.request(), Action::ExitNow));
        assert!(q.requested());
    }

    #[test]
    fn an_armed_shutdown_runs_once_on_the_requests_thread_and_a_second_request_exits() {
        let q = Quit::new();
        let n = Arc::new(AtomicU32::new(0));
        assert!(q.arm(counting(&n)).is_none());
        let Action::Run(f) = q.request() else { panic!("the request runs it") };
        f();
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert!(q.take().is_none(), "the main thread finds it gone");
        assert!(matches!(q.request(), Action::ExitNow), "a second request cuts it short");
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_main_threads_take_wins_and_a_later_request_exits() {
        let q = Quit::new();
        let n = Arc::new(AtomicU32::new(0));
        assert!(q.arm(counting(&n)).is_none());
        q.take().expect("not yet taken")();
        assert!(matches!(q.request(), Action::ExitNow), "the main thread is already quitting");
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn in_polled_mode_the_main_thread_quits_and_a_close_waits_for_it() {
        let q = Quit::new();
        q.poll();
        assert!(matches!(q.request(), Action::MainQuits));
        assert!(q.requested());
        assert!(!q.wait_quit(Duration::from_millis(20)), "not finished yet");
        let main = {
            let q = q.clone();
            std::thread::spawn(move || {
                q.quitting();
                q.finished();
            })
        };
        assert!(q.wait_quit(Duration::from_secs(5)));
        main.join().expect("main");
        assert!(matches!(q.request(), Action::ExitNow));
    }

    #[test]
    fn arming_after_a_request_hands_the_shutdown_back() {
        let q = Quit::new();
        q.poll();
        assert!(matches!(q.request(), Action::MainQuits));
        let n = Arc::new(AtomicU32::new(0));
        let back = q.arm(counting(&n)).expect("a request came: run it now");
        back();
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert!(q.take().is_none());
    }
}
