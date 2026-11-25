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
use pe_seams::{Frame, FrameView, Hit, HostTime, ObjectId, Pos, Shape};

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
}

#[derive(Default)]
struct HitHistory {
    frames: BTreeMap<Frame, Vec<ObservedHit>>,
}

struct ObservedHit {
    hit: PresentedHit,
    pending: bool,
}

impl HitHistory {
    fn replace(
        &mut self,
        from: Frame,
        end: Frame,
        now: HostTime,
        hits: impl Iterator<Item = (Frame, bool, PresentedHit)>,
    ) {
        let mut previous = BTreeMap::<[ObjectId; 2], Vec<&ObservedHit>>::new();
        for old in self.frames.range(from..end).flat_map(|(_, hits)| hits) {
            previous.entry(old.hit.bodies).or_default().push(old);
        }
        let mut replacement: BTreeMap<Frame, Vec<ObservedHit>> = BTreeMap::new();
        for (frame, resimulated, hit) in hits {
            if now.0.saturating_sub(hit.when.0) > (MAX_REPLAY_AGE_NS + MATCH_NS) as i64 {
                continue;
            }
            let matching = if resimulated {
                previous.get(&hit.bodies).and_then(|old| {
                    old.iter()
                        .filter(|old| old.hit.when.0.abs_diff(hit.when.0) <= MATCH_NS)
                        .min_by_key(|old| old.hit.when.0.abs_diff(hit.when.0))
                })
            } else {
                None
            };
            // A match can still be waiting in this display's batch. Keep that onset, with the corrected hit data.
            let pending = matching.is_none_or(|old| old.pending);
            replacement.entry(frame).or_default().push(ObservedHit { hit, pending });
        }
        // An empty replacement still retracts every old hit in the stepped range.
        self.frames.retain(|frame, _| !(from..end).contains(frame));
        self.frames.append(&mut replacement);
    }

    fn take_pending(&mut self, now: HostTime, hits: &mut Vec<PresentedHit>) {
        self.frames.retain(|_, frame_hits| {
            frame_hits.retain_mut(|entry| {
                let age = now.0.saturating_sub(entry.hit.when.0);
                if std::mem::take(&mut entry.pending) && age <= MAX_REPLAY_AGE_NS as i64 {
                    hits.push(entry.hit);
                }
                age <= (MAX_REPLAY_AGE_NS + MATCH_NS) as i64
            });
            !frame_hits.is_empty()
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
        let mut history = HitHistory::default();
        let mut last_queued = BTreeMap::new();
        let mut domain = None;
        let mut hits = Vec::with_capacity(MAX_HITS_PER_DISPLAY);
        let mut last_fixture = None;
        let mut last_material = Material::Wood;
        Box::new(move |display, view, new, cameras| {
            let Some(listener) = center(cameras) else { return };
            if let Ok(mut state) = self.listener.try_lock() {
                *state = Some(listener);
            }
            let received = Instant::now();
            last_queued.retain(|_, at| received.duration_since(*at) < Duration::from_nanos(MATCH_NS));
            hits.clear();
            let count = if self.fixture {
                if last_fixture.is_none_or(|last| received.duration_since(last) >= Duration::from_millis(450)) {
                    fixture_hit(display, view, listener, last_material, &mut hits);
                    last_fixture = Some(received);
                    last_material = match last_material {
                        Material::Wood => Material::Metal,
                        Material::Metal => Material::Wood,
                    };
                }
                hits.len()
            } else {
                if domain != Some(view.domain) {
                    history.frames.clear();
                    last_queued.clear();
                    domain = Some(view.domain);
                }
                let mut count = 0;
                for update in new.iter().filter(|update| update.domain == view.domain) {
                    if let Some(from) = update.hits_from {
                        count += update.hits.len();
                        history.replace(
                            from,
                            update.frame,
                            view.sim_time,
                            update.hits.iter().map(|hit| (hit.frame, hit.resimulated, physical_hit(update, hit))),
                        );
                    }
                }
                history.take_pending(view.sim_time, &mut hits);
                count
            };
            self.report.hits_received.fetch_add(count as u64, super::RELAXED);
            if !hits.is_empty() {
                self.publish(&mut hits, &mut last_queued, received, listener.pos);
            }
        })
    }

    fn publish(
        &self,
        hits: &mut [PresentedHit],
        last_queued: &mut BTreeMap<[ObjectId; 2], Instant>,
        received: Instant,
        listener: Pos,
    ) {
        hits.sort_unstable_by(|a, b| priority(*b, listener).total_cmp(&priority(*a, listener)));
        let mut accepted = 0;
        let mut dropped = 0;
        for hit in hits {
            if accepted + dropped == MAX_HITS_PER_DISPLAY {
                break;
            }
            if hit.approach_mps.is_finite()
                && hit.approach_mps >= MIN_APPROACH_SPEED_MPS
                && !last_queued.contains_key(&hit.bodies)
            {
                let impact = Impact {
                    at: hit.at,
                    material: hit.material,
                    size_m: hit.size_m,
                    strength: hit.strength,
                    submitted: received,
                };
                if self.impacts.try_send(impact).is_ok() {
                    last_queued.insert(hit.bodies, received);
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

fn physical_hit(view: &FrameView, hit: &Hit) -> PresentedHit {
    let (size_m, material) = impact_profile(view, [hit.part_a, hit.part_b]);
    PresentedHit {
        bodies: pair_key([hit.a, hit.b]),
        when: hit.sim_time,
        at: hit.at,
        approach_mps: hit.speed,
        strength: hit.speed,
        size_m,
        material,
    }
}

fn impact_profile(view: &FrameView, parts: [ObjectId; 2]) -> (f32, Material) {
    // The trial uses shape as a stand-in for a future physics material. S5's radius already includes agreed scale.
    // The plane player root has no physics Body or Shape row. Hand colliders do reach the feed and use the other side.
    parts
        .into_iter()
        .filter_map(|id| {
            let part = view.object(id)?;
            let shape = part.shape?;
            (part.hand_of.is_none() && part.radius.is_finite() && part.radius > 0.0).then_some((
                part.radius * 2.0,
                shape_profile(Some(shape)).1,
                id,
            ))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0).then(a.2.cmp(&b.2)))
        .map_or((0.3, Material::Wood), |(size, material, _)| (size, material))
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
        bodies: pair_key([id, ObjectId::NONE]),
        when: view.sim_time,
        at,
        approach_mps: 1.5,
        strength: 2.0,
        size_m,
        material,
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

    fn hit(a: u128, b: u128, ms: i64) -> PresentedHit {
        PresentedHit {
            bodies: pair_key([ObjectId::from_u128(a), ObjectId::from_u128(b)]),
            when: HostTime::from_millis(ms),
            at: Pos::ORIGIN,
            approach_mps: 1.0,
            strength: 1.0,
            size_m: 0.3,
            material: Material::Wood,
        }
    }

    #[test]
    fn replay_matches_the_pair_in_either_order_and_retracts_missing_hits() {
        let mut history = HitHistory::default();
        let mut pending = Vec::new();
        let now = HostTime::from_millis(1_090);
        history.replace(Frame(60), Frame(65), now, [(Frame(60), false, hit(7, 11, 1000))].into_iter());
        history.take_pending(now, &mut pending);
        assert_eq!(pending.len(), 1);
        pending.clear();

        let replay = hit(11, 7, 1015);
        let new = hit(7, 13, 1015);
        history.replace(Frame(60), Frame(66), now, [(Frame(61), true, replay), (Frame(61), true, new)].into_iter());
        history.take_pending(now, &mut pending);
        assert_eq!(pending.iter().map(|hit| hit.bodies).collect::<Vec<_>>(), [new.bodies]);
        pending.clear();

        history.replace(Frame(60), Frame(67), now, std::iter::empty());
        history.replace(Frame(60), Frame(68), now, [(Frame(61), true, replay)].into_iter());
        history.take_pending(now, &mut pending);
        assert_eq!(pending.iter().map(|hit| hit.bodies).collect::<Vec<_>>(), [replay.bodies]);
    }

    #[test]
    fn rollback_within_one_display_updates_a_pending_hit_and_drops_stale_hits() {
        let mut history = HitHistory::default();
        let mut pending = Vec::new();
        let now = HostTime::from_millis(1_210);
        history.replace(Frame(60), Frame(70), now, [(Frame(64), false, hit(7, 11, 1060))].into_iter());
        let corrected = hit(11, 7, 1075);
        let stale = hit(9, 13, 1000);
        history.replace(
            Frame(60),
            Frame(71),
            now,
            [(Frame(65), true, corrected), (Frame(60), true, stale)].into_iter(),
        );
        history.take_pending(now, &mut pending);
        assert_eq!(pending.iter().map(|hit| hit.when).collect::<Vec<_>>(), [corrected.when]);
    }
}
