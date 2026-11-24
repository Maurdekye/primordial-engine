//! The audio trial's adapter for locally presented hits.

use std::collections::BTreeMap;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glam::DVec3;
use pe_render::camera::Camera;
use pe_render::display::DisplayFrame;
use pe_render::pose::{WorldPose, offset_vec};
use pe_render::present::{AudioObserver, Presenter};
use pe_seams::{FrameView, HostTime, ObjectId, Pos, Shape};

use super::mix::{Impact, Material};

const MATCH_NS: u64 = 45_000_000;
const MAX_REPLAY_AGE_NS: u64 = 180_000_000;
const MAX_HITS_PER_DISPLAY: usize = 64;
const MIN_APPROACH_SPEED_MPS: f32 = 0.35;

pub(super) type Listener = Arc<Mutex<Option<WorldPose>>>;

#[derive(Clone, Copy)]
struct PresentedHit {
    bodies: [ObjectId; 2],
    when: HostTime,
    at: Pos,
    approach_mps: f32,
    strength: f32,
    size_m: f32,
    material: Material,
    resimulated: bool,
}

#[derive(Default)]
struct HitGate {
    recent: BTreeMap<[ObjectId; 2], PlayedPair>,
}

struct PlayedPair {
    times: Vec<HostTime>,
    last_queued: Instant,
}

impl HitGate {
    fn eligible(&self, hit: PresentedHit, now: HostTime, received: Instant) -> bool {
        let age = now.0.saturating_sub(hit.when.0).max(0) as u64;
        if !hit.approach_mps.is_finite()
            || hit.approach_mps < MIN_APPROACH_SPEED_MPS
            || !hit.strength.is_finite()
            || hit.strength <= 0.0
            || (hit.resimulated && age > MAX_REPLAY_AGE_NS)
        {
            return false;
        }
        self.recent.get(&pair_key(hit.bodies)).is_none_or(|played| {
            received.saturating_duration_since(played.last_queued) >= Duration::from_nanos(MATCH_NS)
                && played.times.iter().all(|when| when.0.abs_diff(hit.when.0) > MATCH_NS)
        })
    }

    fn record(&mut self, hit: PresentedHit, received: Instant) {
        let played = self
            .recent
            .entry(pair_key(hit.bodies))
            .or_insert_with(|| PlayedPair { times: Vec::new(), last_queued: received });
        played.last_queued = received;
        played.times.push(hit.when);
    }

    fn prune(&mut self, now: HostTime, received: Instant) {
        self.recent.retain(|_, played| {
            played.times.retain(|at| now.0.saturating_sub(at.0) <= (MAX_REPLAY_AGE_NS + MATCH_NS) as i64);
            !played.times.is_empty()
                || received.saturating_duration_since(played.last_queued) < Duration::from_nanos(MATCH_NS)
        });
    }
}

fn pair_key(mut bodies: [ObjectId; 2]) -> [ObjectId; 2] {
    bodies.sort_unstable();
    bodies
}

pub(super) struct Publisher {
    pub listener: Listener,
    pub impacts: SyncSender<Impact>,
    pub fixture: bool,
    pub report: Arc<super::StreamCounters>,
}

impl Publisher {
    pub fn attach(self, presenter: &mut Presenter) {
        presenter.set_audio_observer(self.observer());
    }

    fn observer(self) -> AudioObserver {
        let mut gate = HitGate::default();
        let mut hits = Vec::with_capacity(MAX_HITS_PER_DISPLAY);
        let mut last_fixture = None;
        let mut last_material = Material::Wood;
        Box::new(move |display, view, _new, cameras| {
            let Some(listener) = center(cameras) else { return };
            if let Ok(mut state) = self.listener.try_lock() {
                *state = Some(listener);
            }
            if !self.fixture {
                return;
            }
            let now = pe_app::clock::now();
            let received = Instant::now();
            gate.prune(now, received);
            hits.clear();
            if last_fixture.is_none_or(|last| received.duration_since(last) >= Duration::from_millis(450)) {
                fixture_hit(display, view, listener, now, last_material, &mut hits);
                last_fixture = Some(received);
                last_material = match last_material {
                    Material::Wood => Material::Metal,
                    Material::Metal => Material::Wood,
                };
            }
            self.publish(&mut hits, &mut gate, now, received, listener.pos);
        })
    }

    fn publish(&self, hits: &mut [PresentedHit], gate: &mut HitGate, now: HostTime, received: Instant, listener: Pos) {
        let count = hits.len();
        hits.sort_unstable_by(|a, b| priority(*b, listener).total_cmp(&priority(*a, listener)));
        self.report.hits_received.fetch_add(count as u64, super::RELAXED);
        let mut accepted = 0;
        let mut dropped = 0;
        for hit in hits {
            if accepted + dropped == MAX_HITS_PER_DISPLAY {
                break;
            }
            if gate.eligible(*hit, now, received) {
                let impact = Impact {
                    at: hit.at,
                    material: hit.material,
                    size_m: hit.size_m,
                    strength: hit.strength,
                    submitted: received,
                };
                if self.impacts.try_send(impact).is_ok() {
                    gate.record(*hit, received);
                    accepted += 1;
                } else {
                    dropped += 1;
                }
            }
        }
        self.report.hits_queued.fetch_add(accepted as u64, super::RELAXED);
        self.report.queue_drops.fetch_add(dropped as u64, super::RELAXED);
    }
}

fn priority(hit: PresentedHit, listener: Pos) -> f64 {
    (hit.strength.max(0.0) as f64).sqrt() / (1.0 + offset_vec(hit.at - listener).length())
}

fn center(cameras: &[Camera]) -> Option<WorldPose> {
    let first = cameras.first()?.pose;
    Some(cameras.get(1).map_or(first, |second| {
        first.moved(offset_vec(second.pose.pos - first.pos) * 0.5).with_rot(first.rot.slerp(second.pose.rot, 0.5))
    }))
}

fn fixture_hit(
    display: &DisplayFrame,
    view: &FrameView,
    listener: WorldPose,
    now: HostTime,
    material: Material,
    hits: &mut Vec<PresentedHit>,
) {
    // Exercise the same queue with a presented hand or object and a tone
    // fixture. Neither writes an event into the world or the render effect gate.
    let source = display
        .objects
        .iter()
        .filter_map(|object| {
            let meta = view.object(object.id)?;
            let distance = offset_vec(object.pose.pos - listener.pos).length();
            let (size_m, kind) = shape_profile(meta.shape);
            (distance > 0.25 && kind == material && (meta.shape.is_some() || meta.hand_of.is_some())).then_some((
                object,
                distance,
                size_m,
                meta.hand_of.is_some(),
            ))
        })
        .min_by(|a, b| b.3.cmp(&a.3).then(a.1.total_cmp(&b.1)));
    let (id, at, size_m) = source.map_or_else(
        || {
            let side = match material {
                Material::Wood => -1.0,
                Material::Metal => 1.0,
            };
            let offset = listener.rot * DVec3::new(side, 0.0, -1.5);
            (ObjectId::from_u128(1), listener.moved(offset).pos, 0.3)
        },
        |(object, _, size_m, _)| (object.id, object.pose.pos, size_m),
    );
    hits.push(PresentedHit {
        bodies: [id, ObjectId::NONE],
        when: now,
        at,
        approach_mps: 1.5,
        strength: 2.0,
        size_m,
        material,
        resimulated: false,
    });
}

fn shape_profile(shape: Option<Shape>) -> (f32, Material) {
    match shape {
        Some(Shape::Box { half_extents }) => (2.0 * half_extents.into_iter().fold(0.0, f32::max), Material::Wood),
        Some(Shape::Sphere { radius }) => (2.0 * radius, Material::Metal),
        Some(Shape::Capsule { radius, half_height }) => (2.0 * (radius + half_height), Material::Metal),
        _ => (0.3, Material::Wood),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_matches_the_pair_in_either_order_and_drops_stale_hits() {
        let started = Instant::now();
        let when = HostTime::from_millis(1_000);
        let hit = PresentedHit {
            bodies: [ObjectId::from_u128(7), ObjectId::from_u128(11)],
            when,
            at: Pos::ORIGIN,
            approach_mps: 1.0,
            strength: 1.0,
            size_m: 0.3,
            material: Material::Wood,
            resimulated: false,
        };
        let mut gate = HitGate::default();
        assert!(gate.eligible(hit, when, started));
        gate.record(hit, started);

        let replay = PresentedHit {
            bodies: [hit.bodies[1], hit.bodies[0]],
            when: HostTime::from_millis(1_015),
            resimulated: true,
            ..hit
        };
        let later = started + Duration::from_millis(90);
        assert!(!gate.eligible(replay, HostTime::from_millis(1_090), later));
        let unmatched = PresentedHit { bodies: [hit.bodies[0], ObjectId::from_u128(13)], ..replay };
        assert!(gate.eligible(unmatched, HostTime::from_millis(1_090), later));
        assert!(!gate.eligible(unmatched, HostTime::from_millis(1_200), later));
        let next = PresentedHit { when: HostTime::from_millis(1_088), ..hit };
        assert!(!gate.eligible(next, next.when, started + Duration::from_millis(29)));
        assert!(gate.eligible(next, next.when, later));
    }
}
