//! Primordial Engine fork (D3): swept bodies, pe-world's bullets (world-model.md 3.2, "The bullet flag").
//!
//! A body with [`body_flags::SWEPT`] is swept in every sub-step against every shape it collides with, whatever that
//! shape's body does: static, kinematic or dynamic, awake or asleep, or another swept body. Box3D's own continuous
//! collision (`continuous.rs`) rewinds a fast body to its first time of impact against static shapes along the step's
//! chord, after the step. This pass works inside each sub-step, on the motion the sub-step solved:
//!
//! 1. Before `integrate positions` ([`SweptPass::find`]): each swept body's swept box over the sub-step, widened by how
//!    far any awake body can have moved since the step began, finds its candidate shapes in the broad-phase trees. The
//!    bodies of the candidate pairs keep their poses at the sub-step's start.
//! 2. After `integrate positions` ([`SweptPass::resolve`]): conservative advancement finds each pair's earliest time
//!    of impact along both bodies' pieces of motion, turns included, stepping only by a lower bound on their distance
//!    (a separating plane's gap; GJK's witness distance is an upper bound). The earliest impact over all pairs is
//!    handled first: both bodies move to it, take a two-body impulse at the contact point (the pair's mixed friction
//!    and restitution), and their pairs are swept again on the rest of the sub-step. A swept body deflected out of
//!    every box a search covered is searched again, and every one again when a body's reach grows. A candidate a
//!    search turned away because the two paths didn't meet is looked at again when either body's piece changes.
//! 3. A fixed number of distance queries per sub-step ([`World::swept_iteration_budget`]) bounds the work. When it
//!    runs out, every body connected through candidate pairs to a pair not yet proven safe is held at the set's last
//!    time proven safe.
//!
//! A pair that begins a piece within its contact distance (resting, sliding, or just answered) is swept by the
//! touching rule on its cores, the shapes moved inward by a share of their inner size ([`core_proxy`]): resting and
//! sliding contact stays the contact solver's, and the cores never overlap, so the shapes never pass.
//!
//! A sleeping body can't join the solver within a step: it stays where it is, each impact on it adds to a pending
//! velocity that later impacts in the step see, and the total is applied once when its island wakes at the step's end
//! ([`finish`]). A moving body pressing into it is held for the rest of the step (@coordinator-opus).
//!
//! Serial on worker 0 between stages, in body-id, shape-id and pair order, with the fork's own arithmetic, so the
//! results don't depend on the worker count (fork invariant D2). With no swept body nothing here runs, so such a world
//! steps bit-identically to one built without this file.

use super::StepContext;
use crate::body::{body_flags, should_bodies_collide, wake_body, BodyState};
use crate::constants::{linear_slop, MAX_ROTATION};
use crate::core::NULL_INDEX;
use crate::distance::{make_proxy, shape_distance, DistanceInput, ShapeProxy, SimplexCache};
use crate::dynamic_tree::DEFAULT_MASK_BITS;
use crate::events::ContactHitEvent;
use crate::id::{ContactId, ShapeId};
use crate::hull::{get_hull_planes, get_hull_points};
use crate::math_functions::{
    aabb_inflate, aabb_overlaps, aabb_union, abs_float, add, clamp_float, clamp_int, conjugate, cross, dot,
    integrate_rotation, inv_mul_transforms, inv_rotate_vector, inv_transform_point, length, length_squared, lerp,
    max_float, min_float, mul_add, mul_mv, mul_quat, mul_sv, nlerp, normalize, offset_aabb, offset_pos, rotate_vector,
    solve3, sub, sub_pos, transform_point, Aabb, Matrix3, Plane, Pos, Quat, Transform, Vec3, MAT3_ZERO,
    TRANSFORM_IDENTITY, VEC3_ZERO,
};
use crate::mesh::{
    get_mesh_material_indices, get_mesh_triangle, query_mesh, Mesh, CONCAVE_EDGE1, CONCAVE_EDGE2, CONCAVE_EDGE3,
};
use crate::shape::{
    compute_shape_aabb, compute_shape_min_extent, make_shape_proxy, shape_flags, should_shapes_collide, Shape,
    ShapeGeometry,
};
use crate::solver_set::{AWAKE_SET, DISABLED_SET, FIRST_SLEEPING_SET};
use crate::types::BodyType;
use crate::world::World;

/// The most distance queries one sub-step may spend on all its swept pairs, the same on every machine. A constant of
/// the build for now; pe-physics passes it in before each step (`World::swept_iteration_budget`), so a later world
/// setting can default old worlds to this value (@coordinator-opus, Q3).
pub const SWEPT_ITERATION_BUDGET: i32 = 1024;

/// How far the touching rule's core lies inside a shape of inner size `e` (its `min_extent`): a quarter of it, but at
/// least 0.7 linear slop so that two cores stay apart in Box3D's resting contact (which overlaps by about a slop),
/// and at most half of it, so a core is left.
fn shrink(e: f32) -> f32 {
    min_float(0.5 * e, max_float(0.25 * e, 0.7 * linear_slop()))
}

/// What the sweeps did in the latest step, summed over its sub-steps. Not snapshotted or hashed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweptCounters {
    /// Candidate pairs swept, once per sub-step each.
    pub pairs: i32,
    /// Pieces left to the contact solver unswept: the pair can't close a slop over them.
    pub slow: i32,
    /// Pieces proven apart without a query: the bodies' bounding spheres never meet over them (a mesh triangle's own
    /// sphere for a triangle pair), or the bullet stays in front of a triangle's plane.
    pub culled: i32,
    /// Pieces of a mesh triangle pair left alone: the bullet's centre is behind the triangle, and Box3D's triangles
    /// are one-sided, so its contact solver doesn't collide them either.
    pub behind: i32,
    /// Distance queries: the budget's unit.
    pub iterations: i32,
    /// Impacts answered by an impulse.
    pub hits: i32,
    /// Pieces swept under the touching rule: the pair began the piece within its contact distance.
    pub touching: i32,
    /// Pieces not swept because the pair began them deeper than the touching rule allows and wasn't approaching.
    pub embedded: i32,
    /// Bodies held at a time proven safe: the budget ran out, the distance couldn't be bounded from below, or a body
    /// pressed into a sleeping one.
    pub holds: i32,
    /// Sub-steps whose budget ran out.
    pub exhausted: i32,
    /// Impulses on sleeping bodies, applied when their islands wake at the step's end.
    pub deferred: i32,
    /// Searches run again after a deflection left every box a search covered, or a body's reach grew.
    pub researches: i32,
    /// Body pairs a search had turned away, looked at again after a piece changed.
    pub reconsidered: i32,
}

/// [`Level::Deep`]'s band: a quarter slop, or a quarter of the thinner core's half-extent if that is less.
fn deep_band(thin: f32, tol: f32) -> f32 {
    min_float(tol, 0.25 * thin)
}

/// [`Level::Deep`]'s target for cores `upper` apart (negative when they overlap): two bands below the cores, or below
/// touching if they're apart, never below `-floor`.
fn deep_target(upper: f32, floor: f32, thin: f32, tol: f32) -> f32 {
    max_float(min_float(upper, 0.0) - 2.0 * deep_band(thin, tol), -floor)
}

/// Keeps [`World::swept_bodies`] in step with the `SWEPT` flag: adds or removes `body_id`, in increasing order.
pub(crate) fn list_swept_body(world: &mut World, body_id: i32, on: bool) {
    let list = &mut world.swept_bodies;
    match (list.binary_search(&body_id), on) {
        (Err(at), true) => list.insert(at, body_id),
        (Ok(at), false) => {
            list.remove(at);
        }
        _ => {}
    }
}

/// Rebuilds [`World::swept_bodies`] from the bodies' flags, after a snapshot restore.
pub(crate) fn rebuild_swept_list(world: &mut World) {
    world.swept_bodies = world
        .bodies
        .iter()
        .filter(|b| b.id != NULL_INDEX && (b.flags & body_flags::SWEPT) != 0)
        .map(|b| b.id)
        .collect();
}

/// One body taking part in this sub-step's sweeps.
#[derive(Clone, Copy)]
struct Mover {
    body_id: i32,
    /// Its index in the awake set's states, or `NULL_INDEX` when it doesn't move this step (static or asleep).
    state_index: i32,
    asleep: bool,
    /// Kinematic or static: takes no impulse.
    fixed: bool,
    /// The body is swept and its shapes allow it.
    swept: bool,
    inv_mass: f32,
    inv_inertia: Matrix3,
    local_center: Vec3,
    /// The state's flags (locks, `ALLOW_FAST_ROTATION`).
    flags: u32,
    /// Its centre of mass and rotation at the step's start; the `d` offsets below are from `center`.
    center: Pos,
    q_step: Quat,
    /// The current piece of the sub-step: from fraction `t0` at (`d1`, `q1`) to fraction 1 at (`d2`, `q2`), at
    /// velocity (`v`, `w`). `d` is the centre of mass's offset from `center`, `q` the world rotation.
    t0: f32,
    d1: Vec3,
    q1: Quat,
    d2: Vec3,
    q2: Quat,
    v: Vec3,
    w: Vec3,
    /// A sleeping body's pending velocity from this step's impacts so far.
    dv: Vec3,
    dw: Vec3,
    /// Stopped at `t0` for the rest of the sub-step.
    held: bool,
    /// Its piece changed this sub-step: the state is written back.
    changed: bool,
    /// How many new pieces an impulse or a separation gave it this sub-step ([`Culled::other_pieces`]).
    pieces: u32,
    /// The heads of its lists of [`Culled`] entries: those its own searches made, and those naming it as `other`.
    culled_by: u32,
    culled_from: u32,
    /// The latest search that kept an entry naming it as `other`: one entry a search.
    culled_mark: u32,
    /// How far its farthest shape point is from its centre of mass.
    radius: f32,
}

impl Mover {
    /// Centre offset and rotation at sub-step fraction `t` (at or after `t0`).
    fn pose(&self, t: f32) -> (Vec3, Quat) {
        if self.held || t <= self.t0 {
            return (self.d1, self.q1);
        }
        if self.t0 >= 1.0 || t >= 1.0 {
            return (self.d2, self.q2);
        }
        let u = min_float((t - self.t0) / (1.0 - self.t0), 1.0);
        (lerp(self.d1, self.d2, u), nlerp(self.q1, self.q2, u))
    }

    /// The body's transform at `t`, relative to `center`.
    fn transform(&self, t: f32) -> Transform {
        let (d, q) = self.pose(t);
        Transform { p: sub(d, rotate_vector(q, self.local_center)), q }
    }

    /// The centre's velocity on the piece, per unit of sub-step fraction.
    fn rate(&self) -> Vec3 {
        if self.held || self.t0 >= 1.0 {
            VEC3_ZERO
        } else {
            mul_sv(1.0 / (1.0 - self.t0), sub(self.d2, self.d1))
        }
    }

    /// A bound on the piece's turn rate per unit of sub-step fraction. The piece turns by angle `a` from `q1` to `q2`
    /// along `nlerp`, whose rate peaks at 4 tan(a/4) = 4 |r.v| / (1 + |r.s|), `r` the relative rotation.
    fn turn_rate(&self) -> f32 {
        if self.held || self.t0 >= 1.0 {
            return 0.0;
        }
        let r = mul_quat(conjugate(self.q1), self.q2);
        4.0 * length(r.v) / (1.0 + r.s.abs()) / (1.0 - self.t0)
    }

    /// The velocity an impact sees: a sleeping body's is its pending one.
    fn velocity(&self) -> (Vec3, Vec3) {
        (add(self.v, self.dv), add(self.w, self.dw))
    }
}

/// What a sweep found at `t`.
#[derive(Clone, Copy, PartialEq)]
enum Event {
    /// The pair reached its target: the contact point (relative to mover `a`'s `center`) and the normal from `a` to
    /// `b`.
    Hit { point: Vec3, normal: Vec3 },
    /// The distance couldn't be bounded from below closely enough to go on: both bodies are held here.
    Uncertain,
    /// At Deep's floor and not provable: each body keeps only its motion away from the other along `normal` (from
    /// `a` to `b`), which never shortens their distance.
    Separate { normal: Vec3 },
}

/// A candidate pair: a swept body's shape and a shape it collides with, or one triangle of a mesh it collides with.
#[derive(Clone)]
struct Pair {
    /// Movers; `a`'s body id is below `b`'s, except in a triangle pair, whose `a` is the swept body and `b` the mesh's.
    a: usize,
    b: usize,
    shape_a: i32,
    shape_b: i32,
    proxy_a: ShapeProxy,
    proxy_b: ShapeProxy,
    /// The shapes' cores for the touching rule ([`core_proxy`]).
    core_a: ShapeProxy,
    core_b: ShapeProxy,
    /// Each proxy's farthest point from its body's centre of mass, its radius included.
    radius_a: f32,
    radius_b: f32,
    friction: f32,
    restitution: f32,
    /// Proven free of contact up to this fraction on the bodies' current pieces.
    proven: f32,
    /// What the current piece is swept on, the distance it's swept to, and how near that distance counts as
    /// reaching it ([`SweptPass::sweep`]).
    level: Level,
    target: f32,
    band: f32,
    /// The thinner core's least half-extent: crossing it takes twice this.
    thin: f32,
    /// How far the cores may ever overlap at [`Level::Deep`]: nine tenths of the way across the thinner core
    /// (`1.8 thin`), and nine tenths of the cores' radii's sum, the deepest overlap GJK can measure.
    floor: f32,
    /// The next sweep starts a piece: its target is classified first.
    fresh: bool,
    /// Nothing more to sweep on the current pieces: proven to 1, or left alone on this piece.
    done: bool,
    /// The earliest event found on the current pieces, at a fraction.
    event: Option<(f32, Event)>,
    cache: SimplexCache,
    core_cache: SimplexCache,
    /// `b` is a mesh triangle (`proxy_b` and `core_b` are the triangle itself).
    tri: Option<Tri>,
}

/// A mesh triangle as a pair's partner `b` (CCD part 2). Box3D's triangles are one-sided: its contact solver drops a
/// sphere, capsule or hull whose centre is behind one (`manifold/triangle.rs`, `triangle_hull.rs`), so the pair is swept
/// only while the bullet's centre is in front.
#[derive(Clone, Copy)]
struct Tri {
    /// The triangle's index in its mesh, part of the pair's key.
    index: i32,
    /// Its front normal (Box3D's `make_plane_from_points` winding), in `b`'s body frame.
    face: Vec3,
    /// Box3D's edge flags as `get_mesh_triangle` gives them: an edge concave or flat from the front has no edge normal.
    flags: i32,
    /// Its centroid in `b`'s body frame, the radius of its bounding sphere about it, and the centroid's distance from
    /// `b`'s centre of mass.
    centroid: Vec3,
    reach: f32,
    arm: f32,
    /// The bullet shape's centre in `a`'s body frame, which Box3D's back-side cull tests, and how far behind the plane
    /// that cull lets it be (a hull's slop).
    centre: Vec3,
    margin: f32,
}

/// Whether the triangle feature GJK's simplex `cache` ends on (its `b` indices: one vertex, two an edge, three the face)
/// is smooth from the front: the face, an edge that `flags` mark concave or flat, or a vertex both of whose edges in
/// this triangle are. There the impact's normal is the face's, as Box3D's contact solver drops an inner edge's contact,
/// so a bullet sliding across a flat mesh isn't bumped at its triangles' edges.
fn smooth_feature(cache: &SimplexCache, flags: i32) -> bool {
    let mut on = [false; 3];
    for k in 0..(cache.count as usize).min(4) {
        let i = cache.index_b[k] as usize;
        if i < 3 {
            on[i] = true;
        }
    }
    let concave = |edges: i32| (flags & edges) == edges;
    match on {
        [true, true, true] => true,
        [true, true, false] => concave(CONCAVE_EDGE1),
        [false, true, true] => concave(CONCAVE_EDGE2),
        [true, false, true] => concave(CONCAVE_EDGE3),
        [true, false, false] => concave(CONCAVE_EDGE1 | CONCAVE_EDGE3),
        [false, true, false] => concave(CONCAVE_EDGE1 | CONCAVE_EDGE2),
        [false, false, true] => concave(CONCAVE_EDGE2 | CONCAVE_EDGE3),
        [false, false, false] => false,
    }
}

/// What a pair's piece is swept on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Level {
    /// The shapes: their point clouds to Box3D's time-of-impact distance (a slop inside their surfaces).
    Full,
    /// The touching rule: the pair began the piece within its contact distance, so its cores are swept, to a quarter
    /// slop apart, then after a graze there to touching, then deeper ([`Level::Deep`]) but never past the pair's
    /// `floor`. Crossing the thinner core takes twice its half-extent, more than `floor`, so the shapes never pass.
    Core,
    Touch,
    /// The cores touch or overlap (a graze or a hit at touching, such as a rod pivoting on its tip, or the contact
    /// solver turning a body out of a deep contact): swept to one band below where they are, never past `floor`,
    /// so the sweep steps past a graze to the next approach.
    Deep,
}

/// One distance query of a pair at a level.
struct Measure {
    /// An upper and a lower bound on the distance the level measures: the point clouds' for [`Level::Full`], the
    /// cores' surfaces' (negative when they overlap) for the touching levels.
    upper: f32,
    lower: f32,
    /// The lower bound along `normal` alone (when `apart`), for the closing bound along it.
    lower_n: f32,
    /// The contact point between the surfaces, relative to mover `a`'s centre, and the normal from `a` to `b`.
    point: Vec3,
    normal: Vec3,
    /// The point clouds are apart, so the normal is one.
    apart: bool,
}

/// Why a sweep stopped.
enum Stop {
    /// An event is recorded on the pair, or it's proven or left alone to the end.
    Swept,
    /// The budget ran out.
    Exhausted,
}

/// A sleeping body's pending velocity, kept for the rest of the step.
#[derive(Clone, Copy)]
struct Pending {
    body_id: i32,
    dv: Vec3,
    dw: Vec3,
}

/// A candidate a search turned away because the two bodies' paths' boxes didn't meet then ([`SweptPass::consider`]).
/// A later piece of either body can meet the other's, so the pair is looked at again when one of them changes
/// ([`SweptPass::reconsider`]). Kept only when `other` is awake and dynamic: no other body's piece changes within a
/// sub-step.
#[derive(Clone, Copy)]
struct Culled {
    /// The swept mover whose search turned it away, and the mover whose shape that search found.
    searcher: usize,
    other: usize,
    /// `other`'s [`Mover::pieces`] when it was turned away.
    other_pieces: u32,
    /// The next entry with the same `searcher`, and the next with the same `other` ([`NO_ENTRY`]: none).
    next_by_searcher: u32,
    next_by_other: u32,
}

/// The end of a mover's list of [`Culled`] entries.
const NO_ENTRY: u32 = u32::MAX;

/// The pass of one step: [`SweptPass::find`] and [`SweptPass::resolve`] around each sub-step's `integrate positions`,
/// then [`finish`] after the step.
pub(super) struct SweptPass {
    movers: Vec<Mover>,
    pairs: Vec<Pair>,
    /// Body id to mover index plus one (0: none), for the current sub-step.
    mover_of: Vec<u32>,
    /// Sleeping bodies' pending velocities, in body-id order.
    pending: Vec<Pending>,
    /// Bodies held for the rest of the step because they pressed into a sleeping body, in body-id order.
    held_for_step: Vec<i32>,
    /// How far any awake body can have moved from its broad-phase box by the end of the sub-step.
    reach: f32,
    /// This sub-step's searches since the reach last grew: the swept mover and the box searched (world frame, before
    /// the reach). Each box was queried whole, so a path inside it can meet only shapes that query found: each is a
    /// candidate, or was turned away on paths that met nowhere and is kept in `culled` while the other body's piece
    /// can still change. A box bounding two of them was not queried whole (the corner between two legs of a
    /// deflected path).
    searched: Vec<(usize, Aabb)>,
    /// This sub-step's turned-away candidates since the reach last grew ([`Culled`]).
    culled: Vec<Culled>,
    /// Counts this sub-step's searches, for [`Mover::culled_mark`].
    search_serial: u32,
    /// The pair the next sub-step's sweep starts at (taken modulo the pairs' count): just after the pair whose sweep
    /// ran out of budget in the last one, so that an overload lasting through a step holds a different set each
    /// sub-step, and a pair whose sweep alone needs more than the budget never takes it from the others. A step's first sub-step
    /// starts at the world's step index, which the snapshot holds, so this needs no state of its own. A `u64`, reduced
    /// in `u64`, so that every platform starts at the same pair.
    start: u64,
    budget: i32,
    /// `resolve` is under way: the awake states already hold the sub-step's end.
    resolving: bool,
    h: f32,
    max_linear_speed: f32,
    max_angular_speed: f32,
    restitution_threshold: f32,
    hit_threshold: f32,
    /// The step's impacts that were hard enough to report, in the order they were answered ([`finish`]).
    hits: Vec<ContactHitEvent>,
    counters: SweptCounters,
}

impl SweptPass {
    pub(super) fn new(world: &World, context: &StepContext) -> SweptPass {
        SweptPass {
            movers: Vec::new(),
            pairs: Vec::new(),
            mover_of: vec![0; world.bodies.len()],
            pending: Vec::new(),
            held_for_step: Vec::new(),
            reach: 0.0,
            searched: Vec::new(),
            culled: Vec::new(),
            search_serial: 0,
            start: world.step_index,
            budget: 0,
            resolving: false,
            h: context.h,
            max_linear_speed: context.max_linear_velocity,
            max_angular_speed: MAX_ROTATION * context.inv_dt,
            restitution_threshold: world.restitution_threshold,
            hit_threshold: world.hit_event_threshold,
            hits: Vec::new(),
            counters: SweptCounters::default(),
        }
    }

    /// Before `integrate positions`: the candidate pairs of every swept body over this sub-step, and the poses their
    /// bodies start it at. `states` are the awake states, still at the sub-step's start.
    pub(super) fn find(&mut self, world: &World, states: &[BodyState]) {
        for m in &self.movers {
            self.mover_of[m.body_id as usize] = 0;
        }
        self.movers.clear();
        self.pairs.clear();
        self.searched.clear();
        self.culled.clear();
        self.search_serial = 0;
        self.budget = world.swept_iteration_budget;
        self.resolving = false;

        // The reach: how far any awake body's points can be from where its broad-phase box was made, by the end of
        // this sub-step: the chord moved since the step began plus this sub-step's motion, at full speed.
        let awake = &world.solver_sets[AWAKE_SET as usize];
        let mut reach = 0.0f32;
        for (sim, state) in awake.body_sims.iter().zip(states) {
            let r = length(sim.max_extent);
            let moved = length(state.delta_position) + 2.0 * length(state.delta_rotation.v) * r;
            let next = self.h * (length(state.linear_velocity) + length(state.angular_velocity) * r);
            reach = max_float(reach, moved + next);
        }
        self.reach = reach;

        for i in 0..world.swept_bodies.len() {
            let body_id = world.swept_bodies[i];
            if let Some(m) = self.mover(world, states, body_id) {
                if self.movers[m].swept {
                    self.search(world, states, m, None);
                }
            }
        }
    }

    /// The mover for `body_id`, made the first time a sub-step asks for it; `None` for a disabled body.
    fn mover(&mut self, world: &World, states: &[BodyState], body_id: i32) -> Option<usize> {
        let slot = self.mover_of[body_id as usize];
        if slot != 0 {
            return Some(slot as usize - 1);
        }
        let body = &world.bodies[body_id as usize];
        if body.set_index == DISABLED_SET {
            return None;
        }
        let sim = &world.solver_sets[body.set_index as usize].body_sims[body.local_index as usize];
        let awake = body.set_index == AWAKE_SET;
        let asleep = body.set_index >= FIRST_SLEEPING_SET;
        let fixed = body.type_ != BodyType::Dynamic;
        let swept =
            (body.flags & body_flags::SWEPT) != 0 && body.type_ != BodyType::Static && swept_shapes(world, body_id);
        let (d2, q2, v, w, flags) = if awake {
            let s = &states[body.local_index as usize];
            (s.delta_position, mul_quat(s.delta_rotation, sim.transform.q), s.linear_velocity, s.angular_velocity, s.flags)
        } else {
            (VEC3_ZERO, sim.transform.q, VEC3_ZERO, VEC3_ZERO, 0)
        };
        // A body first met during `resolve` (a search after a deflection) is already at the sub-step's end: its start
        // is taken back along its velocity. A pair it joins begins at the deflection's time, so only its pose from
        // there on matters, and that is its own path to within rounding.
        let (d1, q1) = if awake && self.resolving {
            (mul_add(d2, -self.h, v), integrate_rotation(q2, mul_sv(-self.h, w)))
        } else {
            (d2, q2)
        };
        let (dv, dw) = match self.pending.binary_search_by_key(&body_id, |p| p.body_id) {
            Ok(k) => (self.pending[k].dv, self.pending[k].dw),
            Err(_) => (VEC3_ZERO, VEC3_ZERO),
        };
        let held = awake && self.held_for_step.binary_search(&body_id).is_ok();
        let mut radius = 0.0f32;
        let mut shape_id = body.head_shape_id;
        while shape_id != NULL_INDEX {
            let shape = &world.shapes[shape_id as usize];
            radius = max_float(radius, proxy_radius(&any_proxy(shape), sim.local_center));
            shape_id = shape.next_shape_id;
        }
        self.movers.push(Mover {
            body_id,
            state_index: if awake { body.local_index } else { NULL_INDEX },
            asleep,
            fixed,
            swept,
            inv_mass: if fixed { 0.0 } else { sim.inv_mass },
            inv_inertia: if fixed { MAT3_ZERO } else { sim.inv_inertia_world },
            local_center: sim.local_center,
            flags,
            center: sim.center,
            q_step: sim.transform.q,
            t0: 0.0,
            d1,
            q1,
            d2: if held { d1 } else { d2 },
            q2: if held { q1 } else { q2 },
            v,
            w,
            dv,
            dw,
            held,
            changed: held && self.resolving,
            pieces: 0,
            culled_by: NO_ENTRY,
            culled_from: NO_ENTRY,
            culled_mark: 0,
            radius,
        });
        let index = self.movers.len() - 1;
        self.mover_of[body_id as usize] = index as u32 + 1;
        Some(index)
    }

    /// The box mover `m`'s shapes sweep from fraction `from` to the end of the sub-step, in the world frame. Before
    /// `integrate positions` (`predicted`) the piece isn't known yet: the box takes the state's velocity at full
    /// length, which the speed caps and locks only shorten. A turn adds its chord's bound on every side.
    fn path_box(&self, world: &World, m: usize, from: f32, predicted: bool) -> Aabb {
        offset_aabb(self.path_box_rel(world, m, from, predicted), self.movers[m].center)
    }

    /// [`Self::path_box`] relative to the mover's `center`.
    fn path_box_rel(&self, world: &World, m: usize, from: f32, predicted: bool) -> Aabb {
        let mv = &self.movers[m];
        let (d0, q0) = mv.pose(from);
        let (shift, turn) = if mv.held || mv.state_index == NULL_INDEX {
            (VEC3_ZERO, 0.0)
        } else if predicted {
            (mul_sv(self.h, mv.v), self.h * length(mv.w) * mv.radius)
        } else {
            let r = mul_quat(conjugate(q0), mv.q2);
            (sub(mv.d2, d0), 2.0 * length(r.v) * mv.radius)
        };
        let xf = Transform { p: sub(d0, rotate_vector(q0, mv.local_center)), q: q0 };
        let mut out: Option<Aabb> = None;
        let mut shape_id = world.bodies[mv.body_id as usize].head_shape_id;
        while shape_id != NULL_INDEX {
            let shape = &world.shapes[shape_id as usize];
            let start = compute_shape_aabb(shape, xf);
            let end = Aabb { lower_bound: add(start.lower_bound, shift), upper_bound: add(start.upper_bound, shift) };
            let b = aabb_union(start, end);
            out = Some(out.map_or(b, |o| aabb_union(o, b)));
            shape_id = shape.next_shape_id;
        }
        aabb_inflate(out.unwrap_or_default(), turn)
    }

    /// A box in mover `n`'s body frame holding every point of `rel` (a box relative to mover `m`'s `center`, fixed in
    /// the world) as `n` moves over the rest of the sub-step from `from`: the box mapped at `n`'s pose there, widened
    /// by how far `n`'s centre of mass can go and its turn times the box's farthest point from it. At a later pose a
    /// point's image moves by at most the translation plus |R(s) - R(from)| times its distance from the centre of mass,
    /// and the turn bounds that rotation's chord (@astra-deliberator: a 1 m crossing during an 11.25 degree turn strays
    /// 49 mm from the chord, where the sagitta alone gives 2.4 mm).
    fn local_box(&self, m: usize, n: usize, rel: Aabb, from: f32, predicted: bool) -> Aabb {
        let (ma, mb) = (&self.movers[m], &self.movers[n]);
        let (d0, q0) = mb.pose(from);
        let (shift, turn) = if mb.held || mb.state_index == NULL_INDEX {
            (0.0, 0.0)
        } else if predicted {
            (self.h * length(mb.v), self.h * length(mb.w))
        } else {
            (length(sub(mb.d2, d0)), 2.0 * length(mul_quat(conjugate(q0), mb.q2).v))
        };
        // The box relative to `n`'s centre, then about `n`'s centre of mass there.
        let to_n = sub_pos(ma.center, mb.center);
        let c = add(mul_sv(0.5, add(rel.lower_bound, rel.upper_bound)), to_n);
        let e = mul_sv(0.5, sub(rel.upper_bound, rel.lower_bound));
        let far = length(sub(c, d0)) + length(e);
        // Into the body frame: the centre exactly, the half-extents through the rotation's absolute matrix.
        let local_c = add(inv_rotate_vector(q0, sub(c, d0)), mb.local_center);
        let axes = [Vec3 { x: 1.0, y: 0.0, z: 0.0 }, Vec3 { x: 0.0, y: 1.0, z: 0.0 }, Vec3 { x: 0.0, y: 0.0, z: 1.0 }];
        let mut local_e = VEC3_ZERO;
        for (k, axis) in axes.iter().enumerate() {
            let column = inv_rotate_vector(q0, *axis);
            let w = [e.x, e.y, e.z][k];
            local_e.x += abs_float(column.x) * w;
            local_e.y += abs_float(column.y) * w;
            local_e.z += abs_float(column.z) * w;
        }
        let local = Aabb { lower_bound: sub(local_c, local_e), upper_bound: add(local_c, local_e) };
        aabb_inflate(local, shift + turn * far + linear_slop())
    }

    /// Finds swept mover `m`'s candidate pairs over the rest of the sub-step from `from` (`None`: from its start,
    /// before `integrate positions`) and adds the new ones.
    fn search(&mut self, world: &World, states: &[BodyState], m: usize, from: Option<f32>) {
        let predicted = from.is_none();
        let t = from.unwrap_or(0.0);
        let path = self.path_box(world, m, t, predicted);
        self.searched.push((m, path));
        self.search_serial += 1;
        let wide = aabb_inflate(path, self.reach);
        let mut found: Vec<i32> = Vec::new();
        for (tree, query) in [(BodyType::Static, path), (BodyType::Kinematic, wide), (BodyType::Dynamic, wide)] {
            world.broad_phase.trees[tree as usize].query(query, DEFAULT_MASK_BITS, false, |_proxy, user_data| {
                found.push(user_data as i32);
                true
            });
        }
        found.sort_unstable();
        found.dedup();

        let body_id = self.movers[m].body_id;
        let mut shape_id = world.bodies[body_id as usize].head_shape_id;
        while shape_id != NULL_INDEX {
            let own = &world.shapes[shape_id as usize];
            if own.sensor_index == NULL_INDEX && convex(own) {
                for &other in &found {
                    self.consider(world, states, m, shape_id, other, t, path, predicted, true);
                }
            }
            shape_id = own.next_shape_id;
        }
    }

    /// Adds the pair of swept mover `m`'s `shape_id` and shape `other` if they collide at all and their paths' boxes
    /// overlap. A pair found after the sub-step's start begins at `t`: before it, both bodies were on pieces an
    /// earlier search covered, or that an earlier look turned away. A search (`keep`) keeps what its path test turns
    /// away ([`Culled`]).
    #[allow(clippy::too_many_arguments)]
    fn consider(
        &mut self,
        world: &World,
        states: &[BodyState],
        m: usize,
        shape_id: i32,
        other: i32,
        t: f32,
        path: Aabb,
        predicted: bool,
        keep: bool,
    ) {
        let own = &world.shapes[shape_id as usize];
        let shape = &world.shapes[other as usize];
        let body_id = self.movers[m].body_id;
        let other_body = shape.body_id;
        let mesh = matches!(shape.geometry, ShapeGeometry::Mesh { .. });
        if other_body == body_id || shape.sensor_index != NULL_INDEX || !(convex(shape) || mesh) {
            return;
        }
        if !should_shapes_collide(own.filter, shape.filter) {
            return;
        }
        let (type_a, type_b) = (world.bodies[body_id as usize].type_, world.bodies[other_body as usize].type_);
        if type_a != BodyType::Dynamic && type_b != BodyType::Dynamic {
            return;
        }
        if !should_bodies_collide(world, body_id, other_body) {
            return;
        }
        if (own.flags & shape_flags::ENABLE_CUSTOM_FILTERING) != 0
            || (shape.flags & shape_flags::ENABLE_CUSTOM_FILTERING) != 0
        {
            if let Some(fcn) = world.custom_filter_fcn {
                let id = |s: &Shape| ShapeId { index1: s.id + 1, world0: world.world_id, generation: s.generation };
                if !fcn(world, id(own), id(shape), world.custom_filter_context) {
                    return;
                }
            }
        }
        if mesh {
            self.consider_mesh(world, states, m, shape_id, other, t, predicted, keep);
            return;
        }
        // One pair per shape pair, whichever of its bodies found it.
        let (sa, sb) = if body_id < other_body { (shape_id, other) } else { (other, shape_id) };
        if self.pairs.iter().any(|p| p.tri.is_none() && p.shape_a == sa && p.shape_b == sb) {
            return;
        }
        let Some(n) = self.mover(world, states, other_body) else { return };
        let other_path = self.path_box(world, n, t, predicted);
        if !aabb_overlaps(path, other_path) {
            if keep {
                self.keep_culled(m, n);
            }
            return;
        }
        let (a, b) = if body_id < other_body { (m, n) } else { (n, m) };
        let (shape_a, shape_b) = (&world.shapes[sa as usize], &world.shapes[sb as usize]);
        let (proxy_a, proxy_b) = (make_shape_proxy(shape_a), make_shape_proxy(shape_b));
        let (ma, mb) = (&self.movers[a], &self.movers[b]);
        let (e_a, e_b) =
            (compute_shape_min_extent(shape_a, ma.local_center), compute_shape_min_extent(shape_b, mb.local_center));
        let (core_a, core_b) = (core_proxy(shape_a, e_a), core_proxy(shape_b, e_b));
        let thin = max_float(0.0, min_float(e_a - shrink(e_a), e_b - shrink(e_b)));
        let (mat_a, mat_b) = (shape_a.get_material(0), shape_b.get_material(0));
        let friction = world.friction_callback.unwrap_or(crate::world::default_friction_callback)(
            mat_a.friction,
            mat_a.user_material_id,
            mat_b.friction,
            mat_b.user_material_id,
        );
        let restitution = world.restitution_callback.unwrap_or(crate::world::default_restitution_callback)(
            mat_a.restitution,
            mat_a.user_material_id,
            mat_b.restitution,
            mat_b.user_material_id,
        );
        let (radius_a, radius_b) = (proxy_radius(&proxy_a, ma.local_center), proxy_radius(&proxy_b, mb.local_center));
        self.pairs.push(Pair {
            a,
            b,
            shape_a: sa,
            shape_b: sb,
            proxy_a,
            proxy_b,
            core_a,
            core_b,
            radius_a,
            radius_b,
            friction,
            restitution,
            proven: t,
            level: Level::Full,
            target: 0.0,
            band: 0.25 * linear_slop(),
            thin,
            floor: min_float(1.8 * thin, 0.9 * (core_a.radius + core_b.radius)),
            fresh: true,
            done: false,
            event: None,
            cache: SimplexCache::default(),
            core_cache: SimplexCache::default(),
            tri: None,
        });
        self.counters.pairs += 1;
    }

    /// [`Self::consider`] for a mesh shape `other` (CCD part 2): one pair per triangle whose box meets swept mover
    /// `m`'s path from `t`, mapped into the mesh body's frame over the mesh body's own motion ([`Self::local_box`]),
    /// in triangle order. The triangle, as Box3D's mesh contact takes it (`get_mesh_triangle`: the shape's scale, a
    /// mirroring scale's winding, the edge flags), is the pair's partner on the mesh body's motion. While the mesh
    /// body's piece can still change, the search keeps the body pair ([`Culled`]), so a new piece of either body
    /// queries the triangles again.
    #[allow(clippy::too_many_arguments)]
    fn consider_mesh(
        &mut self,
        world: &World,
        states: &[BodyState],
        m: usize,
        shape_id: i32,
        other: i32,
        t: f32,
        predicted: bool,
        keep: bool,
    ) {
        let shape = &world.shapes[other as usize];
        let ShapeGeometry::Mesh { data, scale } = &shape.geometry else { return };
        let Some(n) = self.mover(world, states, shape.body_id) else { return };
        if keep {
            self.keep_culled(m, n);
        }
        let own = &world.shapes[shape_id as usize];
        // Box3D's back-side cull tests the shape's centre: a sphere's, a capsule's midpoint, a hull's centre (which
        // may sit a slop behind, its hysteresis).
        let (centre, margin) = match &own.geometry {
            ShapeGeometry::Sphere(sphere) => (sphere.center, 0.0),
            ShapeGeometry::Capsule(capsule) => (mul_sv(0.5, add(capsule.center1, capsule.center2)), 0.0),
            ShapeGeometry::Hull(hull) => (hull.center, linear_slop()),
            _ => return,
        };
        let rel = self.path_box_rel(world, m, t, predicted);
        let local = self.local_box(m, n, rel, t, predicted);
        let view = Mesh { data, scale: *scale };
        let mut found: Vec<i32> = Vec::new();
        query_mesh(&view, local, |_, _, _, index| {
            found.push(index);
            true
        });
        found.sort_unstable();
        found.dedup();
        if found.is_empty() {
            return;
        }
        let (ma, mb) = (self.movers[m], self.movers[n]);
        let proxy_a = make_shape_proxy(own);
        let e_a = compute_shape_min_extent(own, ma.local_center);
        let core_a = core_proxy(own, e_a);
        // A triangle has no thickness: the thinner core is the bullet's.
        let thin = max_float(0.0, e_a - shrink(e_a));
        let radius_a = proxy_radius(&proxy_a, ma.local_center);
        let mat_a = own.get_material(0);
        let indices = get_mesh_material_indices(data);
        for index in found {
            if self.pairs.iter().any(|p| p.shape_a == shape_id && p.shape_b == other && p.tri.is_some_and(|x| x.index == index))
            {
                continue;
            }
            let triangle = get_mesh_triangle(&view, index);
            let [p1, p2, p3] = triangle.vertices;
            let face = normalize(cross(sub(p2, p1), sub(p3, p1)));
            let centroid = mul_sv(1.0 / 3.0, add(add(p1, p2), p3));
            let reach = max_float(length(sub(p1, centroid)), max_float(length(sub(p2, centroid)), length(sub(p3, centroid))));
            let proxy_b = make_proxy(&[p1, p2, p3], 0.0);
            let radius_b = proxy_radius(&proxy_b, mb.local_center);
            let material = if indices.is_empty() { 0 } else { indices[index as usize] as i32 };
            let mat_b = shape.get_material(clamp_int(material, 0, shape.material_count() - 1));
            let friction = world.friction_callback.unwrap_or(crate::world::default_friction_callback)(
                mat_a.friction,
                mat_a.user_material_id,
                mat_b.friction,
                mat_b.user_material_id,
            );
            let restitution = world.restitution_callback.unwrap_or(crate::world::default_restitution_callback)(
                mat_a.restitution,
                mat_a.user_material_id,
                mat_b.restitution,
                mat_b.user_material_id,
            );
            self.pairs.push(Pair {
                a: m,
                b: n,
                shape_a: shape_id,
                shape_b: other,
                proxy_a,
                proxy_b,
                core_a,
                core_b: proxy_b,
                radius_a,
                radius_b,
                friction,
                restitution,
                proven: t,
                level: Level::Full,
                target: 0.0,
                band: 0.25 * linear_slop(),
                thin,
                // Short of the bullet core's centre: the contact solver's one-sided push-out always sees it in front.
                floor: min_float(1.8 * thin, 0.9 * core_a.radius),
                fresh: true,
                done: false,
                event: None,
                cache: SimplexCache::default(),
                core_cache: SimplexCache::default(),
                tri: Some(Tri {
                    index,
                    face,
                    flags: triangle.flags,
                    centroid,
                    reach,
                    arm: length(sub(centroid, mb.local_center)),
                    centre,
                    margin,
                }),
            });
            self.counters.pairs += 1;
        }
    }

    /// After `integrate positions`: the sub-step's motion is in `states`. Sweeps every pair, answers the earliest
    /// impact first, and writes back the bodies whose motion changed.
    pub(super) fn resolve(&mut self, world: &World, states: &mut [BodyState]) {
        self.resolving = true;
        for mv in &mut self.movers {
            if mv.state_index == NULL_INDEX {
                continue;
            }
            let s = &states[mv.state_index as usize];
            mv.d2 = s.delta_position;
            mv.q2 = mul_quat(s.delta_rotation, mv.q_step);
            mv.v = s.linear_velocity;
            mv.w = s.angular_velocity;
            if mv.held {
                mv.d2 = mv.d1;
                mv.q2 = mv.q1;
                mv.changed = true;
            }
        }

        loop {
            let mut exhausted = false;
            // Swept from `start` round the list. Each pair's sweep reads and writes only its own state, and the event
            // answered next is chosen by time and then pair index, so the order changes nothing unless the budget
            // runs out, and then it decides which pairs were proven first.
            let count = self.pairs.len();
            let first = if count > 0 { (self.start % count as u64) as usize } else { 0 };
            for k in 0..count {
                let p = (first + k) % count;
                if self.pairs[p].done || self.pairs[p].event.is_some() {
                    continue;
                }
                if let Stop::Exhausted = self.sweep(p) {
                    exhausted = true;
                    self.start = p as u64 + 1;
                    break;
                }
            }
            if exhausted {
                self.counters.exhausted += 1;
                self.hold_unproven();
                break;
            }
            // The earliest event, ties to the lower pair.
            let mut next: Option<(f32, usize)> = None;
            for (p, pair) in self.pairs.iter().enumerate() {
                if let Some((t, _)) = pair.event {
                    // A match, not `Option::is_none_or` (Rust 1.82): the fork's MSRV is 1.77.
                    let earlier = match next {
                        None => true,
                        Some((best, _)) => t < best,
                    };
                    if earlier {
                        next = Some((t, p));
                    }
                }
            }
            let Some((t, p)) = next else { break };
            self.respond(world, states, p, t);
        }

        for mv in &self.movers {
            if !mv.changed || mv.state_index == NULL_INDEX {
                continue;
            }
            let s = &mut states[mv.state_index as usize];
            s.delta_position = mv.d2;
            s.delta_rotation = mul_quat(mv.q2, conjugate(mv.q_step));
            s.linear_velocity = mv.v;
            s.angular_velocity = mv.w;
        }
    }

    /// Conservative advancement of pair `p` from where it's proven to: one distance query per iteration, each a step
    /// by the distance's lower bound over a bound on how fast the two can close, until the pair reaches its target,
    /// is proven apart to the end of the sub-step, or the budget runs out.
    ///
    /// A piece is first classified. A pair farther apart than Box3D's time-of-impact distance is swept on its shapes
    /// to that distance. A nearer one (resting, sliding, or just answered) is swept by the touching rule on its cores,
    /// which lie inside the shapes ([`core_proxy`]): to a quarter slop apart, then after a graze there to touching,
    /// then two bands below the cores' depth at each graze or hit ([`Level::Deep`]), never past the pair's `floor`. A
    /// pair at the floor is answered once if it approaches; otherwise it's left to the contact solver if nothing of
    /// either can close the rest of the way across the thinner core by the sub-step's end, and held where it is if
    /// something can.
    fn sweep(&mut self, p: usize) -> Stop {
        let slop = linear_slop();
        let tol = 0.25 * slop;
        let gap = 0.25 * slop;
        let (a, b) = (self.pairs[p].a, self.pairs[p].b);
        let (ma, mb) = (self.movers[a], self.movers[b]);
        let offset = sub_pos(mb.center, ma.center);
        // The shapes' farthest points bound their cores' too.
        let closing = length(sub(mb.rate(), ma.rate()))
            + ma.turn_rate() * self.pairs[p].radius_a
            + mb.turn_rate() * self.pairs[p].radius_b;
        let full_target = max_float(slop, self.pairs[p].proxy_a.radius + self.pairs[p].proxy_b.radius - slop);
        // Along a fixed world direction `n`, the gap shrinks no faster than this: the centres' approach along it
        // plus each body's turn rate times its farthest point.
        let (radius_a, radius_b) = (self.pairs[p].radius_a, self.pairs[p].radius_b);
        let along = |n: Vec3| {
            max_float(0.0, -dot(sub(mb.rate(), ma.rate()), n)) + ma.turn_rate() * radius_a + mb.turn_rate() * radius_b
        };
        let mut t = max_float(self.pairs[p].proven, max_float(ma.t0, mb.t0));
        // A pair that can't close a slop, nor the thinner core's half-extent, over the rest of the sub-step can't pass
        // through anything: the contact solver has it, and no query is spent (a bullet resting, rolling or jostling
        // among slow bodies costs nothing). A fast partner, a paddle swinging at a resting ball, still makes it swept.
        if closing * (1.0 - t) <= min_float(slop, self.pairs[p].thin) {
            self.pairs[p].proven = 1.0;
            self.pairs[p].done = true;
            self.counters.slow += 1;
            return Stop::Swept;
        }
        let at = |t: f32| {
            let xa = ma.transform(t);
            let mut xb = mb.transform(t);
            xb.p = add(xb.p, offset);
            (xa, xb)
        };
        // Box3D's triangles are one-sided: its contact solver drops a bullet whose centre is behind one. A triangle
        // pair whose bullet starts the piece there is left alone for the piece, so a bullet arriving from behind
        // passes, as it does through the contact solver.
        if let (Some(tri), true) = (self.pairs[p].tri, self.pairs[p].fresh) {
            let (xa, xb) = at(t);
            let centre = inv_transform_point(xb, transform_point(xa, tri.centre));
            if dot(tri.face, sub(centre, self.pairs[p].proxy_b.points[0])) < -tri.margin {
                self.pairs[p].proven = 1.0;
                self.pairs[p].done = true;
                self.counters.behind += 1;
                return Stop::Swept;
            }
        }
        // A triangle pair whose bullet's point cloud stays in front of the triangle's plane, farther than the shapes'
        // target, over the rest of the piece never reaches the triangle: proven apart without a query. While the mesh
        // body doesn't turn, the plane moves with it linearly and so does the bullet's centre of mass, so the ends
        // of the piece bound the distance; every cloud point is within the cloud's radius of the centre of mass, and
        // a core's points lie inside the shape's hull. A bullet skimming a mesh floor costs nothing this way.
        if let (Some(_), true) = (self.pairs[p].tri, mb.turn_rate() == 0.0) {
            let n = rotate_vector(mb.q1, self.pairs[p].tri.map_or(VEC3_ZERO, |tri| tri.face));
            let corner = self.pairs[p].proxy_b.points[0];
            let (_, xb0) = at(t);
            let (_, xb1) = at(1.0);
            let d0 = dot(n, sub(ma.pose(t).0, transform_point(xb0, corner)));
            let d1 = dot(n, sub(ma.pose(1.0).0, transform_point(xb1, corner)));
            let cloud = self.pairs[p].radius_a - self.pairs[p].proxy_a.radius;
            if min_float(d0, d1) - cloud > full_target + tol {
                self.pairs[p].proven = 1.0;
                self.pairs[p].done = true;
                self.counters.culled += 1;
                return Stop::Swept;
            }
        }
        // Within the pieces both centres move linearly, so the bodies' bounding spheres meet only if the segment their
        // centres' difference traces over the rest of the sub-step comes within the two radii (and a slop) of zero.
        // If it doesn't, the pair is proven apart without a query: most of a fast bullet's candidates are like this.
        // A mesh triangle takes its own sphere, about its centroid: the centroid's chord over the rest of the
        // sub-step, widened by how far the mesh body's turn can carry it off that chord.
        {
            let (da, _) = ma.pose(t);
            let (from, along, radius_b) = match self.pairs[p].tri {
                Some(tri) => {
                    let ((_, xb0), (_, xb1)) = (at(t), at(1.0));
                    let c0 = sub(transform_point(xb0, tri.centroid), da);
                    let c1 = sub(transform_point(xb1, tri.centroid), ma.pose(1.0).0);
                    (c0, sub(c1, c0), tri.reach + tri.arm * mb.turn_rate() * (1.0 - t))
                }
                None => {
                    let (db, _) = mb.pose(t);
                    (add(offset, sub(db, da)), mul_sv(1.0 - t, sub(mb.rate(), ma.rate())), self.pairs[p].radius_b)
                }
            };
            let l2 = length_squared(along);
            let u = if l2 > 0.0 { clamp_float(-dot(from, along) / l2, 0.0, 1.0) } else { 0.0 };
            let nearest = length(mul_add(from, u, along));
            if nearest > self.pairs[p].radius_a + radius_b + slop {
                self.pairs[p].proven = 1.0;
                self.pairs[p].done = true;
                self.counters.culled += 1;
                return Stop::Swept;
            }
        }

        if self.pairs[p].fresh {
            self.pairs[p].fresh = false;
            let (xa, xb) = at(t);
            let Some(full) = self.measure(p, Level::Full, xa, xb) else {
                self.pairs[p].proven = t;
                return Stop::Exhausted;
            };
            if full.upper > full_target + tol {
                self.pairs[p].level = Level::Full;
                self.pairs[p].target = full_target;
                self.pairs[p].band = tol;
            } else {
                self.counters.touching += 1;
                let Some(core) = self.measure(p, Level::Core, xa, xb) else {
                    self.pairs[p].proven = t;
                    return Stop::Exhausted;
                };
                let pair = &mut self.pairs[p];
                if core.upper > gap + tol {
                    pair.level = Level::Core;
                    pair.target = gap;
                    pair.band = tol;
                } else if core.upper > tol {
                    pair.level = Level::Touch;
                    pair.target = 0.0;
                    pair.band = tol;
                } else if core.upper > deep_target(core.upper, pair.floor, pair.thin, tol) + deep_band(pair.thin, tol) {
                    // The cores touch or overlap, with room left: swept to one band below where they are, so the
                    // sweep steps past a graze to the next approach.
                    pair.level = Level::Deep;
                    pair.target = deep_target(core.upper, pair.floor, pair.thin, tol);
                    pair.band = deep_band(pair.thin, tol);
                } else {
                    // The cores overlap down to the floor. Only an approach is answered, once.
                    if core.apart && approach(&ma, &mb, xa, xb, core.point, core.normal) < 0.0 {
                        pair.event = Some((t, Event::Hit { point: core.point, normal: core.normal }));
                        return Stop::Swept;
                    }
                    // The cores' clouds overlap, or the cores overlap deeper than the floor and a band: the bodies
                    // began the piece inside each other (a spawn, a teleport, an edit), which no sweep made (the floor
                    // keeps a swept motion short of it) and none can undo. The contact solver has the pair
                    // (@design-reviewer's M1 on d78fdfc0: held there, a bullet stayed inside a box for good).
                    if !core.apart || core.upper < -(pair.floor + deep_band(pair.thin, tol)) {
                        pair.done = true;
                        self.counters.embedded += 1;
                        return Stop::Swept;
                    }
                    // Not approaching where they meet. If neither can close the rest of the way across the thinner
                    // core by the sub-step's end (judged along the normal, so a slide along the contact isn't
                    // counted as closing), the contact solver has the pair. Otherwise the rest of the motion can't
                    // be proven, so both are held here.
                    // A pair that can't close at all can't get deeper, so only a closing one is judged.
                    let reach = along(core.normal);
                    if reach > 0.0 && reach * (1.0 - t) > 2.0 * pair.thin + core.lower_n {
                        // Not provable. If either body moves away along the normal, it keeps just that motion; else
                        // both are held.
                        let away = dot(mb.rate(), core.normal) > 0.0 || dot(ma.rate(), core.normal) < 0.0;
                        let event = if away { Event::Separate { normal: core.normal } } else { Event::Uncertain };
                        pair.event = Some((t, event));
                        return Stop::Swept;
                    }
                    pair.done = true;
                    self.counters.embedded += 1;
                    return Stop::Swept;
                }
            }
        }

        loop {
            let (xa, xb) = at(t);
            let level = self.pairs[p].level;
            let Some(m) = self.measure(p, level, xa, xb) else {
                self.pairs[p].proven = t;
                return Stop::Exhausted;
            };
            let pair = &mut self.pairs[p];
            if m.upper <= pair.target + pair.band {
                pair.event = Some((t, Event::Hit { point: m.point, normal: m.normal }));
                return Stop::Swept;
            }
            if m.lower <= pair.target {
                pair.event = Some((t, Event::Uncertain));
                return Stop::Swept;
            }
            if closing <= 0.0 {
                pair.proven = 1.0;
                pair.done = true;
                return Stop::Swept;
            }
            // Both steps are safe on their own, so the longer is: the isotropic bound, and the bound along the
            // normal, which a slide along the contact or a separation doesn't shorten.
            let iso = (m.lower - pair.target) / closing;
            let dir = if m.apart {
                let a_n = along(m.normal);
                if a_n > 0.0 { (m.lower_n - pair.target) / a_n } else { f32::INFINITY }
            } else {
                0.0
            };
            t += max_float(iso, dir);
            if t >= 1.0 {
                pair.proven = 1.0;
                pair.done = true;
                return Stop::Swept;
            }
            pair.proven = t;
        }
    }

    /// One distance query of pair `p` at `level`, with `a` at `xa` and `b` at `xb` (both relative to `a`'s centre):
    /// `None` when the budget is spent. GJK's witness distance is an upper bound; the clouds' gap along its normal
    /// and along the witness direction, the larger, is a lower bound (any direction gives one).
    fn measure(&mut self, p: usize, level: Level, xa: Transform, xb: Transform) -> Option<Measure> {
        if self.budget <= 0 {
            return None;
        }
        self.budget -= 1;
        self.counters.iterations += 1;
        let pair = &mut self.pairs[p];
        let cores = level != Level::Full;
        let (pa, pb, cache) = if cores {
            (pair.core_a, pair.core_b, &mut pair.core_cache)
        } else {
            (pair.proxy_a, pair.proxy_b, &mut pair.cache)
        };
        let rel = inv_mul_transforms(xa, xb);
        let input = DistanceInput { proxy_a: pa, proxy_b: pb, transform: rel, use_radii: false };
        let out = shape_distance(&input, cache, None);
        let apart = out.distance > 0.0;
        let (gap, gap_n) = if apart {
            let witness = normalize(sub(out.point_b, out.point_a));
            let along = plane_gap(&pa, &pb, rel, out.normal);
            (max_float(along, plane_gap(&pa, &pb, rel, witness)), along)
        } else {
            (0.0, 0.0)
        };
        // The shapes' level measures the clouds, as Box3D's time of impact does; the cores' levels their surfaces.
        let radii = if cores { pa.radius + pb.radius } else { 0.0 };
        let surface_a = mul_add(out.point_a, pa.radius, out.normal);
        let surface_b = mul_add(out.point_b, -pb.radius, out.normal);
        Some(Measure {
            upper: out.distance - radii,
            lower: gap - radii,
            lower_n: gap_n - radii,
            point: transform_point(xa, lerp(surface_a, surface_b, 0.5)),
            normal: rotate_vector(xa.q, out.normal),
            apart,
        })
    }

    /// Answers pair `p`'s event at fraction `t`.
    fn respond(&mut self, world: &World, states: &[BodyState], p: usize, t: f32) {
        let (_, event) = self.pairs[p].event.take().expect("an event");
        let (a, b) = (self.pairs[p].a, self.pairs[p].b);
        let (point, normal) = match event {
            Event::Hit { point, normal } => (point, normal),
            Event::Uncertain => {
                self.hold(a, t);
                self.hold(b, t);
                self.restart(a, t);
                self.restart(b, t);
                return;
            }
            Event::Separate { normal } => {
                self.separate(a, t, mul_sv(-1.0, normal));
                self.separate(b, t, normal);
                self.restart(a, t);
                self.restart(b, t);
                // The motion apart along the normal can leave every box the bodies' searches covered.
                self.after_deflection(world, states, a, t);
                self.after_deflection(world, states, b, t);
                return;
            }
        };
        let (ma, mb) = (self.movers[a], self.movers[b]);
        let offset = sub_pos(mb.center, ma.center);
        let (da, _) = ma.pose(t);
        let (db, qb) = mb.pose(t);
        // At a mesh triangle's smooth feature (its face, or an edge or a vertex the mesh's flags mark concave or flat
        // from the front), the impulse takes the face's normal, as Box3D's mesh contact drops an inner edge's contact:
        // a bullet sliding across a flat mesh isn't bumped at its triangles' edges.
        let normal = match self.pairs[p].tri {
            Some(tri) => {
                let pair = &self.pairs[p];
                let cache = if pair.level == Level::Full { &pair.cache } else { &pair.core_cache };
                if smooth_feature(cache, tri.flags) {
                    mul_sv(-1.0, rotate_vector(qb, tri.face))
                } else {
                    normal
                }
            }
            None => normal,
        };
        let ra = sub(point, da);
        let rb = sub(point, add(db, offset));
        let (va, wa) = ma.velocity();
        let (vb, wb) = mb.velocity();
        let vpa = add(va, cross(wa, ra));
        let vpb = add(vb, cross(wb, rb));
        let vrel = sub(vpb, vpa);
        let vn = dot(vrel, normal);

        if vn >= 0.0 {
            // Not approaching. A moving body pressing into a sleeping one stays where it is for the rest of the step;
            // otherwise the pair goes on deeper under the touching rule.
            for (sleeper, mover) in [(a, b), (b, a)] {
                if self.movers[sleeper].asleep && self.movers[mover].state_index != NULL_INDEX {
                    let id = self.movers[mover].body_id;
                    if let Err(at) = self.held_for_step.binary_search(&id) {
                        self.held_for_step.insert(at, id);
                    }
                    self.hold(mover, t);
                    self.restart(mover, t);
                }
            }
            let pair = &mut self.pairs[p];
            pair.proven = t;
            pair.fresh = true;
            pair.done = false;
            return;
        }

        // The two-body impulse at the contact point: restitution above the world's threshold, friction in the
        // Coulomb cone.
        let restitution = if -vn > self.restitution_threshold { self.pairs[p].restitution } else { 0.0 };
        let k = |n: Vec3| {
            let (rna, rnb) = (cross(ra, n), cross(rb, n));
            ma.inv_mass + mb.inv_mass + dot(rna, mul_mv(ma.inv_inertia, rna)) + dot(rnb, mul_mv(mb.inv_inertia, rnb))
        };
        let kn = k(normal);
        if kn <= 0.0 {
            let pair = &mut self.pairs[p];
            pair.proven = t;
            pair.fresh = true;
            return;
        }
        let jn = -(1.0 + restitution) * vn / kn;
        let mut impulse = mul_sv(jn, normal);
        let vt = mul_add(vrel, -vn, normal);
        let speed_t = length(vt);
        if speed_t > 0.0 && self.pairs[p].friction > 0.0 {
            let tangent = mul_sv(1.0 / speed_t, vt);
            let kt = k(tangent);
            if kt > 0.0 {
                let jt = max_float(-speed_t / kt, -self.pairs[p].friction * jn);
                impulse = mul_add(impulse, jt, tangent);
            }
        }
        self.counters.hits += 1;
        self.report(world, p, offset_pos(ma.center, point), normal, -vn);
        self.push(a, t, mul_sv(-1.0, impulse), ra);
        self.push(b, t, impulse, rb);
        let pair = &mut self.pairs[p];
        pair.proven = t;
        pair.fresh = true;
        pair.done = false;
        self.after_deflection(world, states, a, t);
        self.after_deflection(world, states, b, t);
    }

    /// Records pair `p`'s impact as a contact hit event when it would be one: either shape asks for hit events and the
    /// approach is at least the world's hit threshold. It has no contact, so its contact id is null.
    fn report(&mut self, world: &World, p: usize, point: Pos, normal: Vec3, approach_speed: f32) {
        let (sa, sb) = (&world.shapes[self.pairs[p].shape_a as usize], &world.shapes[self.pairs[p].shape_b as usize]);
        let wanted = ((sa.flags | sb.flags) & shape_flags::ENABLE_HIT_EVENTS) != 0;
        if !wanted || approach_speed < self.hit_threshold {
            return;
        }
        let id = |s: &Shape| ShapeId { index1: s.id + 1, world0: world.world_id, generation: s.generation };
        self.hits.push(ContactHitEvent {
            shape_id_a: id(sa),
            shape_id_b: id(sb),
            contact_id: ContactId { index1: 0, world0: world.world_id, padding: 0, generation: 0 },
            point,
            normal,
            approach_speed,
            user_material_id_a: sa.get_shape_user_material_id(0, 0),
            user_material_id_b: sb.get_shape_user_material_id(0, self.pairs[p].tri.map_or(0, |tri| tri.index)),
        });
    }

    /// Applies `impulse` at `r` from mover `m`'s centre at fraction `t`: a new piece for an awake dynamic body, the
    /// pending velocity of a sleeping one, nothing for a kinematic or static one.
    fn push(&mut self, m: usize, t: f32, impulse: Vec3, r: Vec3) {
        let mv = self.movers[m];
        if mv.fixed {
            return;
        }
        let dv = mul_sv(mv.inv_mass, impulse);
        let dw = mul_mv(mv.inv_inertia, cross(r, impulse));
        if mv.asleep {
            let id = mv.body_id;
            let at = match self.pending.binary_search_by_key(&id, |q| q.body_id) {
                Ok(at) => at,
                Err(at) => {
                    self.pending.insert(at, Pending { body_id: id, dv: VEC3_ZERO, dw: VEC3_ZERO });
                    at
                }
            };
            self.pending[at].dv = add(self.pending[at].dv, dv);
            self.pending[at].dw = add(self.pending[at].dw, dw);
            self.movers[m].dv = self.pending[at].dv;
            self.movers[m].dw = self.pending[at].dw;
            self.counters.deferred += 1;
            return;
        }
        if mv.state_index == NULL_INDEX {
            return;
        }
        let (mut v, mut w) = (add(mv.v, dv), add(mv.w, dw));
        // The locks and speed caps `integrate positions` applies.
        let f = mv.flags;
        if (f & body_flags::LOCK_LINEAR_X) != 0 {
            v.x = 0.0;
        }
        if (f & body_flags::LOCK_LINEAR_Y) != 0 {
            v.y = 0.0;
        }
        if (f & body_flags::LOCK_LINEAR_Z) != 0 {
            v.z = 0.0;
        }
        if (f & body_flags::LOCK_ANGULAR_X) != 0 {
            w.x = 0.0;
        }
        if (f & body_flags::LOCK_ANGULAR_Y) != 0 {
            w.y = 0.0;
        }
        if (f & body_flags::LOCK_ANGULAR_Z) != 0 {
            w.z = 0.0;
        }
        if length_squared(v) > self.max_linear_speed * self.max_linear_speed {
            v = mul_sv(self.max_linear_speed / length(v), v);
        }
        if length_squared(w) > self.max_angular_speed * self.max_angular_speed
            && (f & body_flags::ALLOW_FAST_ROTATION) == 0
        {
            w = mul_sv(self.max_angular_speed / length(w), w);
        }
        let (d, q) = mv.pose(t);
        let rest = (1.0 - t) * self.h;
        let mv = &mut self.movers[m];
        mv.t0 = t;
        mv.d1 = d;
        mv.q1 = q;
        mv.d2 = mul_add(d, rest, v);
        mv.q2 = integrate_rotation(q, mul_sv(rest, w));
        mv.v = v;
        mv.w = w;
        mv.held = false;
        mv.changed = true;
        mv.pieces += 1;
        self.restart(m, t);
    }

    /// Stops awake mover `m` at fraction `t` for the rest of the sub-step, its velocity kept.
    fn hold(&mut self, m: usize, t: f32) {
        let mv = &mut self.movers[m];
        if mv.state_index == NULL_INDEX || mv.held {
            return;
        }
        let (d, q) = mv.pose(t);
        mv.t0 = t;
        mv.d1 = d;
        mv.q1 = q;
        mv.d2 = d;
        mv.q2 = q;
        mv.held = true;
        mv.changed = true;
        self.counters.holds += 1;
    }

    /// Keeps only awake mover `m`'s motion along `away` (a unit direction) for the rest of the sub-step, from `t`: a
    /// translation, no turn, and none at all if it moves the other way. Its velocity is kept.
    fn separate(&mut self, m: usize, t: f32, away: Vec3) {
        let mv = self.movers[m];
        if mv.state_index == NULL_INDEX || mv.held || mv.fixed || mv.asleep {
            return;
        }
        let along = max_float(0.0, dot(mv.rate(), away)) * (1.0 - t);
        let (d, q) = mv.pose(t);
        let mv = &mut self.movers[m];
        mv.t0 = t;
        mv.d1 = d;
        mv.q1 = q;
        mv.d2 = mul_add(d, along, away);
        mv.q2 = q;
        mv.changed = true;
        mv.pieces += 1;
        self.counters.holds += 1;
    }

    /// Mover `m`'s piece changed at `t`: every pair it's in is swept again from there.
    fn restart(&mut self, m: usize, t: f32) {
        for pair in &mut self.pairs {
            if pair.a == m || pair.b == m {
                pair.proven = t;
                pair.fresh = true;
                pair.done = false;
                pair.event = None;
            }
        }
    }

    /// After mover `m` was deflected or separated at `t`: a swept body's new path is searched again unless one box
    /// searched since the reach last grew contains it, and the candidates turned away on its old path are looked at
    /// again ([`SweptPass::reconsider`]). If any body's new path reaches farther than the reach every search allowed,
    /// the reach grows, the boxes searched and the candidates turned away within the smaller one stop counting, and
    /// every swept body is searched again.
    fn after_deflection(&mut self, world: &World, states: &[BodyState], m: usize, t: f32) {
        let mv = self.movers[m];
        if mv.state_index == NULL_INDEX || mv.held {
            return;
        }
        let (_, q) = mv.pose(t);
        let rotated = mul_quat(q, conjugate(mv.q_step));
        let moved = length(mv.pose(t).0) + 2.0 * length(rotated.v) * mv.radius;
        let ahead = length(sub(mv.d2, mv.d1)) + 2.0 * length(mul_quat(conjugate(mv.q1), mv.q2).v) * mv.radius;
        let reach = moved + ahead;
        if reach > self.reach {
            self.reach = reach;
            self.counters.researches += 1;
            // Those boxes' kinematic and dynamic queries reached only as far as the smaller reach. The searches below
            // find and turn away afresh.
            self.searched.clear();
            self.culled.clear();
            for mover in &mut self.movers {
                mover.culled_by = NO_ENTRY;
                mover.culled_from = NO_ENTRY;
            }
            for s in 0..self.movers.len() {
                if self.movers[s].swept {
                    self.search(world, states, s, Some(t));
                }
            }
            return;
        }
        if mv.swept {
            let path = self.path_box(world, m, t, false);
            let covered = self.searched.iter().any(|&(s, b)| s == m && crate::math_functions::aabb_contains(b, path));
            if !covered {
                self.counters.researches += 1;
                self.search(world, states, m, Some(t));
            }
        }
        self.reconsider(world, states, m, t);
    }

    /// Keeps swept mover `m`'s search's turned-away candidate on mover `n` ([`Culled`]): once a search, and only when
    /// `n` is awake and dynamic, so that its piece can still change.
    fn keep_culled(&mut self, m: usize, n: usize) {
        let other = self.movers[n];
        if other.state_index == NULL_INDEX || other.fixed || other.culled_mark == self.search_serial {
            return;
        }
        let at = self.culled.len() as u32;
        self.culled.push(Culled {
            searcher: m,
            other: n,
            other_pieces: other.pieces,
            next_by_searcher: self.movers[m].culled_by,
            next_by_other: other.culled_from,
        });
        self.movers[m].culled_by = at;
        self.movers[n].culled_from = at;
        self.movers[n].culled_mark = self.search_serial;
    }

    /// Mover `m`'s piece changed at `t`: the candidates turned away on the old pieces are looked at again on the
    /// paths from `t`. Every one naming `m` as `other` is (its old path was what kept them apart), and every one `m`'s
    /// own searches turned away whose other body has had a new piece since. The rest stand: `m`'s new path lies in a
    /// box its searches covered or is searched again, and the other body's path is still inside the one the search
    /// turned away.
    fn reconsider(&mut self, world: &World, states: &[BodyState], m: usize, t: f32) {
        let mut e = self.movers[m].culled_from;
        while e != NO_ENTRY {
            let c = self.culled[e as usize];
            self.consider_bodies(world, states, c.searcher, m, t);
            e = c.next_by_other;
        }
        let mut e = self.movers[m].culled_by;
        while e != NO_ENTRY {
            let c = self.culled[e as usize];
            if self.movers[c.other].pieces != c.other_pieces {
                self.consider_bodies(world, states, m, c.other, t);
            }
            e = c.next_by_searcher;
        }
    }

    /// Considers every pair of swept mover `s`'s shapes and mover `n`'s from `t`, as a search would, keeping nothing
    /// it turns away (the entry that brought it here stands).
    fn consider_bodies(&mut self, world: &World, states: &[BodyState], s: usize, n: usize, t: f32) {
        self.counters.reconsidered += 1;
        let path = self.path_box(world, s, t, false);
        let mut own_id = world.bodies[self.movers[s].body_id as usize].head_shape_id;
        while own_id != NULL_INDEX {
            let own = &world.shapes[own_id as usize];
            if own.sensor_index == NULL_INDEX && convex(own) {
                let mut other_id = world.bodies[self.movers[n].body_id as usize].head_shape_id;
                while other_id != NULL_INDEX {
                    self.consider(world, states, s, own_id, other_id, t, path, false, false);
                    other_id = world.shapes[other_id as usize].next_shape_id;
                }
            }
            own_id = own.next_shape_id;
        }
    }

    /// The budget ran out: every body connected through candidate pairs to a pair not yet proven safe is held at the
    /// earliest time its connected set is proven safe to.
    fn hold_unproven(&mut self) {
        let n = self.movers.len();
        let mut root: Vec<usize> = (0..n).collect();
        fn root_of(root: &mut [usize], mut i: usize) -> usize {
            while root[i] != i {
                root[i] = root[root[i]];
                i = root[i];
            }
            i
        }
        for pair in &self.pairs {
            let (x, y) = (root_of(&mut root, pair.a), root_of(&mut root, pair.b));
            if x != y {
                root[x.max(y)] = x.min(y);
            }
        }
        let mut safe = vec![f32::INFINITY; n];
        let mut open = vec![false; n];
        for pair in &self.pairs {
            let c = root_of(&mut root, pair.a);
            let until = match pair.event {
                Some((t, _)) => t,
                None if pair.done => 1.0,
                None => pair.proven,
            };
            safe[c] = min_float(safe[c], until);
            open[c] |= !pair.done || pair.event.is_some();
        }
        for m in 0..n {
            let c = root_of(&mut root, m);
            if open[c] {
                self.hold(m, safe[c].min(1.0));
            }
        }
        for pair in &mut self.pairs {
            pair.event = None;
            pair.done = true;
        }
    }

}

/// After the step: each sleeping body an impact reached wakes with its island, the normal way, and takes the
/// pending velocity once, before the step publishes.
pub(super) fn finish(world: &mut World, pass: &SweptPass) {
    for p in &pass.pending {
        let body_id = p.body_id;
        if world.bodies[body_id as usize].id == NULL_INDEX {
            continue;
        }
        wake_body(world, body_id);
        let body = &world.bodies[body_id as usize];
        if body.set_index == AWAKE_SET {
            let state = &mut world.solver_sets[AWAKE_SET as usize].body_states[body.local_index as usize];
            state.linear_velocity = add(state.linear_velocity, p.dv);
            state.angular_velocity = add(state.angular_velocity, p.dw);
        }
    }
    // The sweeps' impacts follow Box3D's own hit events of the step.
    world.contact_hit_events.extend_from_slice(&pass.hits);
    world.swept_counters = pass.counters;
}

/// The velocity along `normal` at which pair movers `a` and `b` approach at the contact `point` (negative when
/// closing), with a sleeping body's pending velocity.
fn approach(a: &Mover, b: &Mover, xa: Transform, xb: Transform, point: Vec3, normal: Vec3) -> f32 {
    let ca = add(xa.p, rotate_vector(xa.q, a.local_center));
    let cb = add(xb.p, rotate_vector(xb.q, b.local_center));
    let (va, wa) = a.velocity();
    let (vb, wb) = b.velocity();
    let vpa = add(va, cross(wa, sub(point, ca)));
    let vpb = add(vb, cross(wb, sub(point, cb)));
    dot(sub(vpb, vpa), normal)
}

/// A lower bound on the distance between `proxy_a` and `proxy_b` (both without radii, `b` at `xf` in `a`'s frame):
/// their gap along `n`, the least of `b` along it less the most of `a`. Any direction gives a lower bound.
fn plane_gap(proxy_a: &ShapeProxy, proxy_b: &ShapeProxy, xf: Transform, n: Vec3) -> f32 {
    let mut most_a = f32::NEG_INFINITY;
    for p in &proxy_a.points[..proxy_a.count as usize] {
        most_a = max_float(most_a, dot(n, *p));
    }
    let nb = inv_rotate_vector(xf.q, n);
    let mut least_b = f32::INFINITY;
    for p in &proxy_b.points[..proxy_b.count as usize] {
        least_b = min_float(least_b, dot(nb, *p));
    }
    least_b + dot(n, xf.p) - most_a
}

/// A swept body's shapes must all be convex primitives: spheres, capsules and hulls (a box is a hull). Anything else
/// leaves the flag without effect (world-model.md 3.2).
fn swept_shapes(world: &World, body_id: i32) -> bool {
    let mut shape_id = world.bodies[body_id as usize].head_shape_id;
    let mut any = false;
    while shape_id != NULL_INDEX {
        let shape = &world.shapes[shape_id as usize];
        if shape.sensor_index == NULL_INDEX {
            if !convex(shape) {
                return false;
            }
            any = true;
        }
        shape_id = shape.next_shape_id;
    }
    any
}

/// A shape this part sweeps: a sphere, capsule or hull. A body with a mesh, height field or compound shape is never
/// swept itself; mesh triangles are its partners (part 2 A).
fn convex(shape: &Shape) -> bool {
    matches!(shape.geometry, ShapeGeometry::Sphere(_) | ShapeGeometry::Capsule(_) | ShapeGeometry::Hull(_))
}

/// A shape's core for the touching rule: the shape moved inward by [`shrink`]`(e)` on every side, `e` its inner size.
/// A sphere's or capsule's radius shrinks. A hull's faces move in exactly where every vertex has three faces (a box,
/// any simple polytope): each vertex solves its three planes moved in. Any other hull scales toward its centre, which
/// keeps it inside but moves long faces' ends in further (part 2's hulls will want the exact offset).
fn core_proxy(shape: &Shape, e: f32) -> ShapeProxy {
    let by = shrink(e);
    // A hull's core is rounded: an inner hull `rho` further in, with radius `rho`, so its flat faces are `by` in
    // from the shape's and GJK can measure how far two cores overlap, down to their radii's sum.
    let rho = 0.5 * max_float(e - by, 0.0);
    match &shape.geometry {
        ShapeGeometry::Sphere(sphere) => make_proxy(&[sphere.center], sphere.radius - by),
        ShapeGeometry::Capsule(capsule) => make_proxy(&[capsule.center1, capsule.center2], capsule.radius - by),
        ShapeGeometry::Hull(hull) => {
            let points = get_hull_points(hull);
            let planes = get_hull_planes(hull);
            let size = 1.0 + length(sub(hull.aabb.upper_bound, hull.aabb.lower_bound));
            let mut core: Vec<Vec3> = Vec::with_capacity(points.len());
            for &v in points {
                let on: Vec<&Plane> =
                    planes.iter().filter(|q| (dot(q.normal, v) - q.offset).abs() <= 1.0e-5 * size).collect();
                if on.len() != 3 {
                    break;
                }
                let m = Matrix3 {
                    cx: Vec3 { x: on[0].normal.x, y: on[1].normal.x, z: on[2].normal.x },
                    cy: Vec3 { x: on[0].normal.y, y: on[1].normal.y, z: on[2].normal.y },
                    cz: Vec3 { x: on[0].normal.z, y: on[1].normal.z, z: on[2].normal.z },
                };
                let into = by + rho;
                let moved = Vec3 { x: on[0].offset - into, y: on[1].offset - into, z: on[2].offset - into };
                let c = solve3(m, moved);
                // A corner moves in by at most `into` times the root of 3 at a box's; much more means a sliver.
                if !(c.x.is_finite() && c.y.is_finite() && c.z.is_finite()) || length(sub(c, v)) > 4.0 * into {
                    break;
                }
                core.push(c);
            }
            if core.len() != points.len() {
                let s = max_float(0.0, 1.0 - (by + rho) / max_float(hull.inner_radius, f32::MIN_POSITIVE));
                core = points.iter().map(|&v| add(hull.center, mul_sv(s, sub(v, hull.center)))).collect();
            }
            make_proxy(&core, rho)
        }
        _ => make_shape_proxy(shape),
    }
}

/// A proxy for any shape, for its radius about a centre: a non-convex shape gives its box's corners.
fn any_proxy(shape: &Shape) -> ShapeProxy {
    if convex(shape) {
        return make_shape_proxy(shape);
    }
    let b = compute_shape_aabb(shape, TRANSFORM_IDENTITY);
    let (l, u) = (b.lower_bound, b.upper_bound);
    let corners = [
        Vec3 { x: l.x, y: l.y, z: l.z },
        Vec3 { x: u.x, y: l.y, z: l.z },
        Vec3 { x: l.x, y: u.y, z: l.z },
        Vec3 { x: u.x, y: u.y, z: l.z },
        Vec3 { x: l.x, y: l.y, z: u.z },
        Vec3 { x: u.x, y: l.y, z: u.z },
        Vec3 { x: l.x, y: u.y, z: u.z },
        Vec3 { x: u.x, y: u.y, z: u.z },
    ];
    make_proxy(&corners, 0.0)
}

/// The farthest point of `proxy` (its radius included) from `center`.
fn proxy_radius(proxy: &ShapeProxy, center: Vec3) -> f32 {
    let mut r = 0.0f32;
    for p in &proxy.points[..proxy.count as usize] {
        r = max_float(r, length(sub(*p, center)));
    }
    r + proxy.radius
}
