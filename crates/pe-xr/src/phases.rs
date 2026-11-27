//! Frame-phase timing for the diagnostic log (spec 10.3): how long each step of a display frame took on the CPU, so a
//! frame that runs long shows where it waited. The session times each step of `XrSession::frame` and, with
//! `PE_XR_LOG=1`, writes one line every [`WINDOW`] with each step's median, 95th percentile and maximum, and one line
//! for the whole run at teardown. It never changes what the session does.

use std::time::{Duration, Instant};

/// The steps of a display frame, in order (spec 3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Between the end of one frame and the start of the next: the loop's events and state changes.
    Outside,
    /// `xrWaitFrame`.
    Wait,
    /// `xrBeginFrame`.
    Begin,
    /// Actions, hands, pointers, locomotion and the space correction, before any image is touched.
    Input,
    /// `xrAcquireSwapchainImage` (colour and depth), less the waits.
    Acquire,
    /// `xrWaitSwapchainImage` (colour and depth): the runtime handing the image back.
    ImageWait,
    /// `xrLocateViews` and the own hands at the display time.
    Locate,
    /// The sink's drawing (pe-app's scene build and pe-render's recording and submit).
    Render,
    /// `xrReleaseSwapchainImage` (colour and depth).
    Release,
    /// `xrEndFrame`.
    End,
    /// The sink's work after the frame (captures).
    After,
}

impl Phase {
    pub const ALL: [Phase; 11] = [
        Phase::Outside,
        Phase::Wait,
        Phase::Begin,
        Phase::Input,
        Phase::Acquire,
        Phase::ImageWait,
        Phase::Locate,
        Phase::Render,
        Phase::Release,
        Phase::End,
        Phase::After,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Phase::Outside => "outside",
            Phase::Wait => "wait",
            Phase::Begin => "begin",
            Phase::Input => "input",
            Phase::Acquire => "acquire",
            Phase::ImageWait => "image_wait",
            Phase::Locate => "locate",
            Phase::Render => "render",
            Phase::Release => "release",
            Phase::End => "end",
            Phase::After => "after",
        }
    }
}

const N: usize = Phase::ALL.len();

/// How often a window's line is written.
pub const WINDOW: Duration = Duration::from_secs(5);

/// A step at or over this counts as slow in the run's line.
pub const SLOW: Duration = Duration::from_millis(5);

/// One frame's times, filled as the frame runs.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameTimes {
    pub(crate) d: [Duration; N],
    /// The whole of `XrSession::frame`, from before `xrWaitFrame` to after the sink's after-frame work.
    pub(crate) total: Duration,
}

impl FrameTimes {
    pub fn add(&mut self, phase: Phase, d: Duration) {
        self.d[phase as usize] += d;
    }

    /// Moves `d` of `from`'s time to `to` (an image wait the backend timed inside the acquire).
    pub fn split(&mut self, from: Phase, to: Phase, d: Duration) {
        let d = d.min(self.d[from as usize]);
        self.d[from as usize] -= d;
        self.d[to as usize] += d;
    }
}

/// The run's totals for one step.
#[derive(Clone, Copy, Debug, Default)]
struct RunPhase {
    sum: Duration,
    max: Duration,
    slow: u64,
}

/// The windows' samples and the run's totals.
#[derive(Debug)]
pub struct Phases {
    on: bool,
    window_start: Option<Instant>,
    window: Vec<FrameTimes>,
    run: [RunPhase; N],
    run_total: RunPhase,
    run_frames: u64,
    /// When the last frame ended, for [`Phase::Outside`].
    last_end: Option<Instant>,
}

impl Phases {
    pub fn new(on: bool) -> Self {
        Phases {
            on,
            window_start: None,
            window: Vec::new(),
            run: [RunPhase::default(); N],
            run_total: RunPhase::default(),
            run_frames: 0,
            last_end: None,
        }
    }

    pub fn on(&self) -> bool {
        self.on
    }

    /// The time since the last frame ended, for a frame starting at `start` (zero for the first frame).
    pub fn outside(&self, start: Instant) -> Duration {
        self.last_end.map_or(Duration::ZERO, |e| start.saturating_duration_since(e))
    }

    /// A frame that ran from `start` to `end`. Returns the window's line when a window has passed.
    pub fn frame(&mut self, mut t: FrameTimes, start: Instant, end: Instant) -> Option<String> {
        if !self.on {
            return None;
        }
        t.total = end.saturating_duration_since(start);
        self.last_end = Some(end);
        for (r, d) in self.run.iter_mut().zip(t.d) {
            r.sum += d;
            r.max = r.max.max(d);
            r.slow += u64::from(d >= SLOW);
        }
        self.run_total.sum += t.total;
        self.run_total.max = self.run_total.max.max(t.total);
        self.run_total.slow += u64::from(t.total >= SLOW);
        self.run_frames += 1;
        self.window.push(t);
        let since = *self.window_start.get_or_insert(start);
        if end.saturating_duration_since(since) < WINDOW {
            return None;
        }
        let line = window_line(&self.window, end.saturating_duration_since(since));
        self.window.clear();
        self.window_start = Some(end);
        Some(line)
    }

    /// The whole run's line, at teardown: each step's mean and maximum, and how many frames it took [`SLOW`] or more.
    pub fn run_line(&self) -> Option<String> {
        if !self.on || self.run_frames == 0 {
            return None;
        }
        let n = self.run_frames as f64;
        let one = |name: &str, r: &RunPhase| format!("{name} {:.2}/{:.2}/{}", ms(r.sum) / n, ms(r.max), r.slow);
        let mut parts: Vec<String> = Phase::ALL.iter().map(|p| one(p.name(), &self.run[*p as usize])).collect();
        parts.push(one("frame", &self.run_total));
        Some(format!(
            "frame phases over the run, {} frames, ms mean/max/frames >= {} ms: {}",
            self.run_frames,
            SLOW.as_millis(),
            parts.join(", ")
        ))
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// The median, 95th percentile and maximum of `v`, in milliseconds.
fn spread(mut v: Vec<Duration>) -> (f64, f64, f64) {
    v.sort_unstable();
    let at = |q: f64| ms(v[((v.len() - 1) as f64 * q).round() as usize]);
    (at(0.5), at(0.95), ms(v[v.len() - 1]))
}

fn window_line(frames: &[FrameTimes], span: Duration) -> String {
    let one = |name: &str, v: Vec<Duration>| {
        let (p50, p95, max) = spread(v);
        format!("{name} {p50:.2}/{p95:.2}/{max:.2}")
    };
    let mut parts: Vec<String> =
        Phase::ALL.iter().map(|p| one(p.name(), frames.iter().map(|f| f.d[*p as usize]).collect())).collect();
    parts.push(one("frame", frames.iter().map(|f| f.total).collect()));
    format!(
        "frame phases over {} frames ({:.1} s), ms p50/p95/max: {}",
        frames.len(),
        span.as_secs_f64(),
        parts.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn off_records_nothing_and_writes_nothing() {
        let mut p = Phases::new(false);
        let base = Instant::now();
        let mut t = FrameTimes::default();
        t.add(Phase::Render, Duration::from_millis(50));
        assert_eq!(p.frame(t, base, at(base, 6000)), None);
        assert_eq!(p.run_line(), None);
    }

    #[test]
    fn a_window_line_comes_after_five_seconds_with_each_step() {
        let mut p = Phases::new(true);
        let base = Instant::now();
        let mut lines = Vec::new();
        // 100 frames 55 ms apart, each spending 50 ms in the render and 2 ms waiting for the image.
        for i in 0..100u64 {
            let (start, end) = (at(base, i * 55), at(base, i * 55 + 53));
            let mut t = FrameTimes::default();
            t.add(Phase::Outside, p.outside(start));
            t.add(Phase::Acquire, Duration::from_millis(2));
            t.split(Phase::Acquire, Phase::ImageWait, Duration::from_millis(2));
            t.add(Phase::Render, Duration::from_millis(50));
            if let Some(l) = p.frame(t, start, end) {
                lines.push(l);
            }
        }
        // The window closes at the first frame ending 5 s or more after the first began: frame 90, at 5,003 ms. The
        // next would close at 10,003 ms, past the last frame.
        assert_eq!(lines.len(), 1, "{lines:?}");
        let l = &lines[0];
        // The gap between frames is 2 ms, and zero before the first.
        assert!(l.starts_with("frame phases over 91 frames (5.0 s), ms p50/p95/max: outside 2.00/2.00/2.00, "), "{l}");
        assert!(l.contains("render 50.00/50.00/50.00"), "{l}");
        assert!(l.contains("acquire 0.00/0.00/0.00, image_wait 2.00/2.00/2.00"), "{l}");
        assert!(l.contains("frame 53.00/53.00/53.00"), "{l}");
        let run = p.run_line().unwrap();
        assert!(run.starts_with("frame phases over the run, 100 frames, ms mean/max/frames >= 5 ms: "), "{run}");
        assert!(run.contains("render 50.00/50.00/100"), "{run}");
        assert!(run.contains("image_wait 2.00/2.00/0"), "{run}");
        assert!(run.contains("frame 53.00/53.00/100"), "{run}");
    }

    #[test]
    fn a_split_never_takes_more_than_the_step_had() {
        let mut t = FrameTimes::default();
        t.add(Phase::Acquire, Duration::from_millis(1));
        t.split(Phase::Acquire, Phase::ImageWait, Duration::from_millis(3));
        assert_eq!(
            (t.d[Phase::Acquire as usize], t.d[Phase::ImageWait as usize]),
            (Duration::ZERO, Duration::from_millis(1))
        );
    }
}
