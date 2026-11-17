//! `view_capture`'s port (api.md 8): what pe-api asks the machine's renderer for, and how a request becomes one.
//!
//! pe-api resolves the request against the newest snapshot (object references, positions, names) and keeps the
//! rules: `at` is `latest` only in M1, sizes are bounded, one capture per principal at a time. The renderer resolves
//! each object's pose in the frame it draws (pe-app's adapter over pe-render's capture service), so the camera and
//! the picture agree.

use std::time::Instant;

use pe_core::{ObjectId, Offset, Pos};
use pe_seams::world_api::{
    ApiError, At, AtName, CameraUsed, Capture, CaptureRequest, ErrorCode, From, MemberId, ObjRef, Visible,
};
use serde_json::{Value as Json, json};

use crate::ports::Snapshot;

/// An id as the API writes it.
struct ObjectIdText(ObjectId);

impl std::fmt::Display for ObjectIdText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", pe_seams::world_api::ObjectId::from(self.0))
    }
}
use crate::query::{Named, resolve};
use crate::world::WorldView;

/// The size when the request gives none, and the largest it may give (api.md 8).
pub const SIZE_DEFAULT: [u32; 2] = [768, 432];
pub const SIZE_MAX: [u32; 2] = [1920, 1080];
/// An orbit's angles when the request gives none, in degrees: a little to the side, a little above.
pub const YAW_DEFAULT: f64 = 30.0;
pub const PITCH_DEFAULT: f64 = 20.0;
/// The farthest an orbit stands from its object (api.md 8).
pub const MAX_ORBIT_M: f64 = 10_000.0;
/// At most this many objects in `visible`, largest first.
pub const VISIBLE_MAX: usize = 50;

/// Where the camera is (pe-app maps it onto pe-render's).
#[derive(Clone, Debug, PartialEq)]
pub enum Aim {
    /// This machine's own view.
    Player,
    /// Around an object: `yaw_deg` about world +y from its world +z side (its rotation is ignored), `pitch_deg` above; without a distance, its bounds
    /// frame it.
    Orbit { id: ObjectId, yaw_deg: f64, pitch_deg: f64, distance_m: Option<f64> },
    /// At a point, looking at an object or another point.
    Placed { at: Pos, look_at: LookAt, fov_deg: Option<f64> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum LookAt {
    Object(ObjectId),
    Point(Pos),
}

/// One capture, as pe-api asks for it.
#[derive(Clone, Debug, PartialEq)]
pub struct Order {
    pub aim: Aim,
    pub size: [u32; 2],
    pub highlight: Vec<ObjectId>,
    /// Names to write beside objects (the highlighted ones, with `label: true`).
    pub labels: Vec<(ObjectId, String)>,
    pub visible: bool,
}

/// What the renderer drew.
#[derive(Clone, Debug, PartialEq)]
pub struct Shot {
    pub png: Vec<u8>,
    pub frame: u64,
    pub eye: Pos,
    pub target: Pos,
    pub fov_deg: f64,
    /// Each drawn object's box in pixels, `[left, top, right, bottom]`, when asked for.
    pub visible: Vec<(ObjectId, [f32; 4])>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Refused {
    /// `Aim::Player` on a machine that draws no view of its own.
    NoView,
    /// An object the aim names isn't in the frame drawn.
    NotInView(ObjectId),
    TimedOut,
    /// Nothing can draw here (no adapter at all).
    Unavailable(String),
    /// The camera would stand outside the world's range.
    OutOfRange,
    Failed(String),
}

/// The machine's renderer, as pe-api calls it: blocks until the shot or `deadline`.
pub trait Captures: Send + Sync {
    fn capture(&self, order: Order, deadline: Instant) -> Result<Shot, Refused>;
}

/// The order a request names, read in `view` (the newest snapshot) for caller `me`.
pub fn order(view: &WorldView, me: MemberId, r: &CaptureRequest) -> Result<Order, ApiError> {
    if r.at != At::Named(AtName::Latest) {
        return Err(ApiError::arg("at", "A capture draws the newest frame only (M1); leave `at` out."));
    }
    let size = r.size.unwrap_or(SIZE_DEFAULT);
    if size[0] == 0 || size[1] == 0 || size[0] > SIZE_MAX[0] || size[1] > SIZE_MAX[1] {
        return Err(ApiError::arg("size", format!("A size is [w, h], at least [1, 1] and at most {SIZE_MAX:?}.")));
    }
    let aim = match &r.from {
        None => Aim::Player,
        Some(From::Player { player }) => {
            if player == "me" || MemberId::parse(player) == Some(me) {
                Aim::Player
            } else {
                return Err(ApiError::new(
                    ErrorCode::CaptureUnavailable,
                    "Another player's view can't be drawn yet (M1 has no heads); capture with orbit or at.",
                ));
            }
        }
        Some(From::Orbit { orbit, yaw_deg, pitch_deg, distance_m }) => {
            if distance_m.is_some_and(|d| !(d.is_finite() && d > 0.0 && d <= MAX_ORBIT_M)) {
                return Err(ApiError::arg(
                    "from.distance_m",
                    "A distance is a positive number of metres, at most 10 km.",
                ));
            }
            Aim::Orbit {
                id: one(view, orbit, "from.orbit")?,
                yaw_deg: yaw_deg.unwrap_or(YAW_DEFAULT),
                pitch_deg: pitch_deg.unwrap_or(PITCH_DEFAULT).clamp(-89.0, 89.0),
                distance_m: *distance_m,
            }
        }
        Some(From::Placed { at, look_at, fov_deg }) => {
            if fov_deg.is_some_and(|f| !(f > 1.0 && f < 170.0)) {
                return Err(ApiError::arg("from.fov_deg", "A field of view is between 1 and 170 degrees."));
            }
            let look_at = match look_at {
                Json::String(s) => LookAt::Object(one(view, &ObjRef(s.clone()), "from.look_at")?),
                v => LookAt::Point(world_pos(view, v, "from.look_at")?),
            };
            Aim::Placed { at: world_pos(view, at, "from.at")?, look_at, fov_deg: *fov_deg }
        }
    };
    let mut highlight = vec![];
    for h in &r.highlight {
        highlight.push(one(view, h, "highlight")?);
    }
    let labels = if r.label {
        highlight.iter().map(|id| (*id, view.info((*id).into()).map(|i| i.name).unwrap_or_default())).collect()
    } else {
        vec![]
    };
    Ok(Order { aim, size, highlight, labels, visible: r.visible_list })
}

/// The answer to a request, from the renderer's shot. `visible` objects are named from `view`, the snapshot at
/// request time, so an object created between the request and the drawn frame gets an empty name.
pub fn answer(view: &WorldView, shot: Shot, visible: bool) -> Capture {
    let place = |p: Pos| json!({ "relative_to": "world_origin", "offset": (p - Pos::ORIGIN).0 });
    let visible = visible.then(|| {
        let mut v: Vec<_> = shot
            .visible
            .iter()
            .map(|(id, [l, t, r, b])| {
                let rect = [*l as u32, *t as u32, (r - l).max(0.0) as u32, (b - t).max(0.0) as u32];
                let name = view.info((*id).into()).map(|i| i.name).unwrap_or_default();
                Visible { id: (*id).into(), name, rect }
            })
            .collect();
        // Largest first (api.md 8), ties by id so the order is stable.
        v.sort_by(|a, b| (b.rect[2] * b.rect[3]).cmp(&(a.rect[2] * a.rect[3])).then(a.id.cmp(&b.id)));
        v.truncate(VISIBLE_MAX);
        v
    });
    Capture {
        png: shot.png,
        frame: shot.frame,
        camera: CameraUsed { at: place(shot.eye), look_at: place(shot.target), fov_deg: shot.fov_deg },
        visible_approx: visible.is_some(),
        visible,
    }
}

/// A refusal as the API's error.
pub fn refusal(r: Refused, view: &WorldView) -> ApiError {
    match r {
        Refused::NoView => ApiError::new(
            ErrorCode::CaptureUnavailable,
            "This machine draws no view of its own; capture with orbit or at.",
        ),
        Refused::NotInView(id) => {
            let name = view
                .info(id.into())
                .map(|i| i.name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| ObjectIdText(id).to_string());
            ApiError::arg("from", format!("{name} isn't in the frame being drawn."))
        }
        Refused::TimedOut => ApiError::new(ErrorCode::Timeout, "The capture took longer than its timeout."),
        Refused::Unavailable(why) => ApiError::new(ErrorCode::CaptureUnavailable, why),
        Refused::OutOfRange => ApiError::arg("from", "The camera would stand outside the world's range."),
        Refused::Failed(why) => ApiError::new(ErrorCode::Internal, format!("The capture failed: {why}")),
    }
}

/// The one object `r` names.
fn one(view: &WorldView, r: &ObjRef, field: &str) -> Result<ObjectId, ApiError> {
    match resolve(view, r) {
        Named::One(id) => Ok(id.into()),
        Named::Many(_) => Err(ApiError::arg(field, format!("{:?} names more than one object; use its id.", r.0))),
        _ => Err(ApiError::arg(field, format!("{:?} names no object.", r.0))),
    }
}

/// A position as api.md 4.4 writes it, in the world: `{"pos": {...}}` exactly, or `{"relative_to", "offset"}` from
/// the world origin or an object (in that object's frame).
fn world_pos(view: &WorldView, v: &Json, field: &str) -> Result<Pos, ApiError> {
    let num3 = |v: Option<&Json>| -> Option<[f64; 3]> {
        let a = v?.as_array()?;
        let n: Option<Vec<f64>> = a.iter().map(Json::as_f64).collect();
        <[f64; 3]>::try_from(n?).ok()
    };
    let range = || ApiError::arg(field, "That position is outside the world's range.");
    if let Some(p) = v.get("pos") {
        let chunk: Option<Vec<i64>> = p
            .get("chunk")
            .and_then(Json::as_array)
            .and_then(|a| a.iter().map(|c| c.as_str().and_then(|s| s.parse().ok()).or_else(|| c.as_i64())).collect());
        let (Some(chunk), Some(offset)) = (chunk.and_then(|c| <[i64; 3]>::try_from(c).ok()), num3(p.get("offset")))
        else {
            return Err(ApiError::arg(
                field,
                "An exact pos is {\"chunk\": [\"i\", \"j\", \"k\"], \"offset\": [x, y, z]}.",
            ));
        };
        return Pos::at_chunk(chunk).and_then(|c| c.checked_add(Offset(offset))).map_err(|_| range());
    }
    let Some(off) = num3(v.get("offset")) else {
        return Err(ApiError::arg(
            field,
            "A position is {\"relative_to\": frame, \"offset\": [x, y, z]}, or an object.",
        ));
    };
    match v.get("relative_to").and_then(Json::as_str) {
        None | Some("world_origin") => Pos::ORIGIN.checked_add(Offset(off)).map_err(|_| range()),
        Some("parent") => Err(ApiError::arg(field, "A camera has no parent; name an object or world_origin.")),
        Some(r) => {
            let Some(Some(f)) = view.frame_of(&ObjRef(r.into())) else {
                return Err(ApiError::arg(field, format!("relative_to {r:?} names no single object.")));
            };
            let fw = view
                .snapshot()
                .world_transform(f)
                .ok_or_else(|| ApiError::arg(field, format!("{r} has no place in the world.")))?;
            let unit = pe_sim::Transform { pos: off, ..pe_sim::Transform::IDENTITY };
            fw.compose(&unit).map(|t| t.pos).ok_or_else(range)
        }
    }
}
