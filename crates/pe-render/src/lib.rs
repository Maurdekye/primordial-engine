//! pe-render (WS-render, @ballpit-viz). Rendering: the window, the scene renderer drawing from frame views (S5), the
//! display layer, the desktop hand, offscreen capture, the GPU context pe-xr uses.
//!
//! Specification: docs/spec/render.md. The seams it implements or uses are in `pe_seams`.
//!
//! - [`drawn_time`]: the drawn time (X8) and resolving simulated poses to it (render.md 7).
//! - [`display`]: the display layer, D1-D5 and D7 (render.md 8).
//! - [`effects`]: one-shot effects fire once, D6.
//! - [`camera`]: camera-relative transforms, reverse-Z projections, view volumes (render.md 5).
//! - [`pose`]: world poses as `Pos` plus an f64 rotation.

pub mod camera;
pub mod display;
pub mod drawn_time;
pub mod effects;
pub mod pose;

pub use pe_seams;

use pe_seams::{HostTime, RigPose};

/// A hand of the local player as presented this display frame: late-latched by pe-xr in VR, or the desktop hand's
/// current input pose (render.md 8, D4).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayHand {
    /// In the rig frame.
    pub pose: RigPose,
}

/// What the local player's presentation gives a display frame (render.md 5).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LocalPresentation {
    /// When this frame is shown: the runtime's predicted display time in VR, now plus one refresh period on the
    /// desktop.
    pub display_time: HostTime,
    /// Indexed by `Hand::index()`.
    pub hands: [Option<DisplayHand>; 2],
}
