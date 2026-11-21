//! The capture port's render side (render.md 11; api.md 8): orders from WorldApi's `view_capture`, drawn by whichever
//! render loop runs on this machine (the window, the headset, or pe-app's headless thread).
//!
//! - **[`CaptureQueue`]** is shared: a caller (pe-app's adapter of pe-api's port) puts an [`Order`] in and blocks
//!   until its [`Shot`] or the deadline. Orders past their deadline are answered `TimedOut` and never drawn.
//! - **[`CaptureServer`]** belongs to one render loop, which calls [`CaptureServer::serve`] once a frame, after its
//!   present. It starts at most one capture at a time, drawn from that frame's newest view at its **simulated** poses
//!   ([`Scene::simulated`]), and collects the readback on a later call, so a capture never stalls a present.
//! - **Aims** are resolved here, against the same view the image is drawn from, so the camera and the picture agree:
//!   an orbit frames an object by its bounding radius, a placed camera looks at an object's or a point's position.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use glam::{DMat3, DQuat, DVec3};
use pe_seams::{FrameView, ObjectId, Offset, Pos};

use crate::camera::{Camera, DepthRange, Fov};
use crate::capture::{CaptureError, PendingCapture, Visible, visible_list};
use crate::counters::CaptureEvent;
use crate::labels::{LabelPlace, place_labels};
use crate::pose::WorldPose;
use crate::scene::{Renderer, Scene};

/// The vertical field of view of an orbit, and of a placed camera without one, in degrees.
pub const DEFAULT_FOV_DEG: f64 = 50.0;
/// An orbit without a distance stands this many bounding radii from the object's centre, at least
/// [`MIN_ORBIT_M`] metres.
pub const ORBIT_RADII: f64 = 3.0;
pub const MIN_ORBIT_M: f64 = 0.5;

/// Where the camera is.
#[derive(Clone, Debug, PartialEq)]
pub enum Aim {
    /// This machine's own view: the window's or the headset's camera, with the capture's aspect.
    Player,
    /// Around an object: `yaw_deg` about world +y from the object's world +z side, `pitch_deg` above its
    /// horizontal plane. World axes: the object's rotation doesn't turn the camera.
    Orbit { id: ObjectId, yaw_deg: f64, pitch_deg: f64, distance_m: Option<f64> },
    /// At a point, looking at an object or another point.
    Placed { at: Pos, look_at: LookAt, fov_deg: Option<f64> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum LookAt {
    Object(ObjectId),
    Point(Pos),
}

/// One `view_capture`, as the render side takes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Order {
    pub aim: Aim,
    pub size: [u32; 2],
    /// Outlined.
    pub highlight: Vec<ObjectId>,
    /// Names written beside these objects (the highlighted ones, when the caller asked for labels).
    pub labels: Vec<(ObjectId, String)>,
    pub visible: bool,
}

/// What a capture drew.
#[derive(Clone, Debug, PartialEq)]
pub struct Shot {
    pub png: Vec<u8>,
    /// The view's frame it was drawn from.
    pub frame: u64,
    pub eye: Pos,
    /// The point the camera looks at.
    pub target: Pos,
    pub fov_deg: f64,
    /// Nearest first, when asked for.
    pub visible: Vec<Visible>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ShotError {
    /// `Aim::Player` on a loop that draws no player's view (headless).
    NoView,
    /// The object an aim names isn't in the view.
    NotInView(ObjectId),
    /// The deadline passed before the image was read back.
    TimedOut,
    /// Nothing can draw here (no graphics adapter at all).
    Unavailable(String),
    /// The camera would stand outside the world's range (an orbit too far out near the world's edge).
    OutOfRange,
    /// The renderer failed (size, device, readback), or the queue is closed.
    Failed(String),
}

struct Reply {
    slot: Mutex<Option<Result<Shot, ShotError>>>,
    cv: Condvar,
}

impl Reply {
    fn send(&self, r: Result<Shot, ShotError>) {
        let mut s = self.slot.lock().expect("the reply");
        if s.is_none() {
            *s = Some(r);
        }
        self.cv.notify_all();
    }
}

struct Job {
    order: Order,
    deadline: Instant,
    reply: Arc<Reply>,
}

#[derive(Default)]
struct Waiting {
    jobs: VecDeque<Job>,
    closed: bool,
}

/// Orders waiting for a render loop.
#[derive(Default)]
pub struct CaptureQueue {
    waiting: Mutex<Waiting>,
    /// Signalled when an order arrives, for a loop that sleeps until one does.
    arrived: Condvar,
}

impl CaptureQueue {
    pub fn new() -> Arc<CaptureQueue> {
        Arc::new(CaptureQueue::default())
    }

    /// Puts `order` in and waits for its shot until `deadline`.
    pub fn request(&self, order: Order, deadline: Instant) -> Result<Shot, ShotError> {
        let reply = Arc::new(Reply { slot: Mutex::new(None), cv: Condvar::new() });
        {
            let mut w = self.waiting.lock().expect("the queue");
            if w.closed {
                return Err(ShotError::Failed("nothing draws on this machine any more".into()));
            }
            w.jobs.push_back(Job { order, deadline, reply: reply.clone() });
        }
        self.arrived.notify_all();
        let mut slot = reply.slot.lock().expect("the reply");
        loop {
            if let Some(r) = slot.take() {
                return r;
            }
            let now = Instant::now();
            if now >= deadline {
                // The server answers it `TimedOut` too when it next looks; the first answer wins.
                return Err(ShotError::TimedOut);
            }
            slot = reply.cv.wait_timeout(slot, deadline - now).expect("the reply").0;
        }
    }

    /// Whether an order is waiting.
    pub fn has_orders(&self) -> bool {
        !self.waiting.lock().expect("the queue").jobs.is_empty()
    }

    /// Sleeps until an order arrives or `timeout` passes; whether one is waiting.
    pub fn wait_for_order(&self, timeout: Duration) -> bool {
        let w = self.waiting.lock().expect("the queue");
        let (w, _) =
            self.arrived.wait_timeout_while(w, timeout, |w| w.jobs.is_empty() && !w.closed).expect("the queue");
        !w.jobs.is_empty()
    }

    /// Refuses every order waiting now with `e` (a loop that can't draw them).
    pub fn refuse_waiting(&self, e: ShotError) {
        let jobs = std::mem::take(&mut self.waiting.lock().expect("the queue").jobs);
        for j in jobs {
            j.reply.send(Err(e.clone()));
        }
    }

    /// Refuses every waiting order and any later one (the loop that drew them has ended).
    pub fn close(&self) {
        let jobs = {
            let mut w = self.waiting.lock().expect("the queue");
            w.closed = true;
            std::mem::take(&mut w.jobs)
        };
        for j in jobs {
            j.reply.send(Err(ShotError::Failed("nothing draws on this machine any more".into())));
        }
        self.arrived.notify_all();
    }

    fn next(&self) -> Option<Job> {
        self.waiting.lock().expect("the queue").jobs.pop_front()
    }
}

struct InFlight {
    job: Job,
    pending: PendingCapture,
    labels: Vec<LabelPlace>,
    frame: u64,
    eye: Pos,
    target: Pos,
    fov_deg: f64,
    visible: Vec<Visible>,
}

/// One render loop's side of the queue.
pub struct CaptureServer {
    queue: Arc<CaptureQueue>,
    in_flight: Option<InFlight>,
}

impl CaptureServer {
    pub fn new(queue: Arc<CaptureQueue>) -> CaptureServer {
        CaptureServer { queue, in_flight: None }
    }

    /// Whether a capture is drawn and waiting for its readback.
    pub fn busy(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Once a frame, after the loop's present: collects a finished readback, then starts the next order, drawn from
    /// `view`. `player`: the loop's own camera, for `Aim::Player` (none on a headless loop). Returns what happened to
    /// orders this call, for the loop's [`crate::present::Presenter::capture_event`] (render.md 13): each order taken
    /// is `Queued`, then `Drawn` once its pass is recorded and submitted, or `TimedOut` if its deadline passes first.
    pub fn serve(&mut self, renderer: &mut Renderer, view: &FrameView, player: Option<&Camera>) -> Vec<CaptureEvent> {
        let mut events = Vec::new();
        if let Some(f) = &self.in_flight {
            let device = renderer.context().device.clone();
            // A readback that finished is kept even when it is collected after the deadline; only one still pending
            // then times out.
            let done = f
                .pending
                .poll(&device)
                .map(|r| r.map_err(|e| ShotError::Failed(format!("{e:?}"))))
                .or_else(|| (Instant::now() >= f.job.deadline).then_some(Err(ShotError::TimedOut)));
            match done {
                None => return events,
                Some(r) => {
                    if matches!(r, Err(ShotError::TimedOut)) {
                        events.push(CaptureEvent::TimedOut);
                    }
                    let f = self.in_flight.take().expect("checked");
                    // Labels and the PNG encode run on a thread of their own: on the headset this is the XR frame
                    // thread, whose frame is 8.3 ms at 120 Hz, and a large encode takes several of those.
                    let finish = move || {
                        f.job.reply.send(r.map(|mut image| {
                            image.draw_labels(&f.labels);
                            Shot {
                                png: image.encode_png(),
                                frame: f.frame,
                                eye: f.eye,
                                target: f.target,
                                fov_deg: f.fov_deg,
                                visible: f.visible,
                            }
                        }))
                    };
                    let shared = Arc::new(Mutex::new(Some(finish)));
                    let worker = shared.clone();
                    let spawned = std::thread::Builder::new().name("pe-capture-encode".into()).spawn(move || {
                        if let Some(finish) = worker.lock().expect("the finish").take() {
                            finish();
                        }
                    });
                    if spawned.is_err() {
                        // No thread to be had: finish here rather than drop the reply.
                        if let Some(finish) = shared.lock().expect("the finish").take() {
                            finish();
                        }
                    }
                }
            }
        }
        while let Some(job) = self.queue.next() {
            events.push(CaptureEvent::Queued);
            if Instant::now() >= job.deadline {
                job.reply.send(Err(ShotError::TimedOut));
                events.push(CaptureEvent::TimedOut);
                continue;
            }
            match self.start(renderer, view, player, job) {
                Ok(()) => {
                    events.push(CaptureEvent::Drawn);
                    return events;
                }
                Err(f) => f.0.reply.send(Err(f.1)),
            }
        }
        events
    }

    fn start(
        &mut self,
        renderer: &mut Renderer,
        view: &FrameView,
        player: Option<&Camera>,
        job: Job,
    ) -> Result<(), Box<(Job, ShotError)>> {
        let o = &job.order;
        let aspect = o.size[0] as f64 / o.size[1].max(1) as f64;
        let aimed = match aim(&o.aim, view, player, aspect) {
            Ok(a) => a,
            Err(e) => return Err(Box::new((job, e))),
        };
        let (camera, target, fov_deg) = aimed;
        let mut scene = Scene::simulated(view);
        scene.highlight(&o.highlight);
        let labels = if o.labels.is_empty() { vec![] } else { place_labels(&scene, &camera, o.size, &o.labels) };
        let visible = if o.visible { visible_list(&scene, &camera, o.size) } else { vec![] };
        let pending = match renderer.capture(&scene, camera, o.size) {
            Ok(p) => p,
            Err(e) => return Err(Box::new((job, failed(e)))),
        };
        self.in_flight = Some(InFlight {
            job,
            pending,
            labels,
            frame: view.frame.0,
            eye: camera.pose.pos,
            target,
            fov_deg,
            visible,
        });
        Ok(())
    }
}

impl Drop for CaptureServer {
    fn drop(&mut self) {
        if let Some(f) = self.in_flight.take() {
            f.job.reply.send(Err(ShotError::Failed("the render loop ended".into())));
        }
    }
}

fn failed(e: CaptureError) -> ShotError {
    match e {
        CaptureError::TimedOut => ShotError::TimedOut,
        e => ShotError::Failed(format!("{e:?}")),
    }
}

/// The camera an aim names in `view`, the point it looks at, and its vertical field of view in degrees.
fn aim(aim: &Aim, view: &FrameView, player: Option<&Camera>, aspect: f64) -> Result<(Camera, Pos, f64), ShotError> {
    let fov = |deg: f64| Fov::symmetric(deg.to_radians() as f32, aspect as f32);
    let object = |id: ObjectId| view.object(id).ok_or(ShotError::NotInView(id));
    match aim {
        Aim::Player => {
            let p = player.ok_or(ShotError::NoView)?;
            let deg = ((p.fov.up - p.fov.down) as f64).to_degrees();
            let ahead = p.pose.rot * DVec3::NEG_Z;
            let target = p.pose.pos.checked_add(Offset(ahead.to_array())).unwrap_or(p.pose.pos);
            Ok((Camera { fov: fov(deg), ..*p }, target, deg))
        }
        Aim::Orbit { id, yaw_deg, pitch_deg, distance_m } => {
            let o = object(*id)?;
            let target = WorldPose::of(o).pos;
            let d = distance_m.unwrap_or((o.radius as f64 * ORBIT_RADII).max(MIN_ORBIT_M));
            let (yaw, pitch) = (yaw_deg.to_radians(), pitch_deg.to_radians());
            let back = DVec3::new(yaw.sin() * pitch.cos(), pitch.sin(), yaw.cos() * pitch.cos()) * d;
            let eye = target.checked_add(Offset(back.to_array())).map_err(|_| out_of_range())?;
            Ok((looking(eye, target, fov(DEFAULT_FOV_DEG)), target, DEFAULT_FOV_DEG))
        }
        Aim::Placed { at, look_at, fov_deg } => {
            let target = match look_at {
                LookAt::Object(id) => WorldPose::of(object(*id)?).pos,
                LookAt::Point(p) => *p,
            };
            let deg = fov_deg.unwrap_or(DEFAULT_FOV_DEG);
            Ok((looking(*at, target, fov(deg)), target, deg))
        }
    }
}

fn out_of_range() -> ShotError {
    ShotError::OutOfRange
}

/// A camera at `eye` looking at `target`, +y up (straight up or down: +z up).
fn looking(eye: Pos, target: Pos, fov: Fov) -> Camera {
    let f = DVec3::from_array((target - eye).0).normalize_or(DVec3::NEG_Z);
    let up = if f.cross(DVec3::Y).length_squared() < 1e-12 { DVec3::Z } else { DVec3::Y };
    let right = f.cross(up).normalize();
    let up = right.cross(f);
    let rot = DQuat::from_mat3(&DMat3::from_cols(right, up, -f));
    Camera::placed(WorldPose { pos: eye, rot }, fov, DepthRange::DESKTOP)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64, z: f64) -> Pos {
        Pos::ORIGIN.checked_add(Offset([x, y, z])).unwrap()
    }

    /// A looking camera puts its target straight ahead (-z in the camera's frame).
    #[test]
    fn a_placed_camera_looks_at_its_target() {
        for (eye, target) in [(p(0.0, 1.0, 3.0), p(0.0, 0.0, 0.0)), (p(2.0, 0.0, 0.0), p(2.0, -5.0, 0.0))] {
            let c = looking(eye, target, Fov::symmetric(1.0, 1.0));
            let r = c.relative(target);
            assert!(r.x.abs() < 1e-9 && r.y.abs() < 1e-9 && r.z < 0.0, "{r:?}");
        }
    }

    /// An order past its deadline is answered `TimedOut` by the waiting caller, and a closed queue refuses.
    #[test]
    fn the_queue_times_out_and_closes() {
        let q = CaptureQueue::new();
        let order = Order { aim: Aim::Player, size: [8, 8], highlight: vec![], labels: vec![], visible: false };
        assert_eq!(q.request(order.clone(), Instant::now()), Err(ShotError::TimedOut));
        assert!(q.has_orders(), "still waiting for a server, which answers it TimedOut");
        q.close();
        assert!(matches!(q.request(order, Instant::now() + Duration::from_secs(1)), Err(ShotError::Failed(_))));
    }
}
