//! Stand-ins for seam S5's XR types, as `docs/spec/xr.md` section 8.1 proposes them and @engine-architect
//! accepted them for pe-seams v0. When pe-seams lands, this module becomes `pub use pe_seams::...` and
//! nothing else in pe-xr changes.

/// Monotonic host nanoseconds (QPC on Windows, `CLOCK_MONOTONIC` on Linux), shared by pe-xr, pe-render
/// and the simulation thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostTime(pub i64);

impl HostTime {
    pub const fn from_millis(ms: i64) -> Self {
        HostTime(ms * 1_000_000)
    }

    /// `self - earlier`, in seconds.
    pub fn secs_since(self, earlier: HostTime) -> f64 {
        (self.0 - earlier.0) as f64 * 1e-9
    }

    pub fn plus_nanos(self, ns: i64) -> Self {
        HostTime(self.0 + ns)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Hand {
    Left,
    Right,
}

impl Hand {
    pub const BOTH: [Hand; 2] = [Hand::Left, Hand::Right];

    pub fn index(self) -> usize {
        match self {
            Hand::Left => 0,
            Hand::Right => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HandSource {
    Tracked,
    Controller,
    Desktop,
}

/// A pose in the local player's rig frame (the tracking space): metres, right-handed, +Y up, -Z forward.
/// The orientation is a unit quaternion in OpenXR's order, (x, y, z, w).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RigPose {
    pub position: [f32; 3],
    pub orientation: [f32; 4],
}

impl RigPose {
    pub const IDENTITY: RigPose = RigPose {
        position: [0.0; 3],
        orientation: [0.0, 0.0, 0.0, 1.0],
    };
}

/// One hand of the local player at one host time (spec 5.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HandInput {
    pub hand: Hand,
    pub source: HandSource,
    pub sampled_at: HostTime,
    /// Whether the pose is valid now. When false, the pose is the last valid one.
    pub tracked: bool,
    pub pose: RigPose,
    pub linear_velocity: Option<[f32; 3]>,
    pub angular_velocity: Option<[f32; 3]>,
    /// 0 open .. 1 closed.
    pub grip: f32,
    /// The grasp, decided on the author's machine with hysteresis.
    pub closed: bool,
    /// Bumped when the pose jumps for a reason other than motion.
    pub discontinuity: u32,
}

/// Written by pe-xr (or pe-render's desktop hand), read by the simulation thread.
pub trait HandFeed {
    /// The local player's hand for a simulation frame's host time; `None` while the hand is absent.
    fn hand_at(&self, hand: Hand, at: HostTime) -> Option<HandInput>;
}
