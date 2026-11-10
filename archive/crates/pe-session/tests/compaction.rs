//! Geometry-registry compaction in the keyframe ring (review P22-12, P2-3 S5): invisible to the
//! simulation. Replicas on different snapshot and compaction policies, one of them rolling back, have
//! equal state hashes at every frame and equal check hashes at every check frame.

use pe_session::session::*;

fn touch(member: MemberId, seq: u32, prev: EntryId, stamp: u64, obj: u32, grab: bool) -> Entry {
    let impulse = [0.3 * (obj % 3) as f32, 1.5, -0.2];
    Entry { member, seq, prev, stamp, payload: Payload::Touch { obj, impulse, grab }, carried: None }
}

fn sim(snap: SnapPolicy, compact: CompactPolicy) -> WorldSim {
    let cfg = SimConfig { snap, compact, check_every: 10, ..SimConfig::default() };
    WorldSim::new(1, 3, &[1, 2, 3], SceneKind::Pile { side: 4, layers: 2 }, cfg, KeyRing::test(3))
}

#[test]
fn compaction_is_invisible_to_the_simulation() {
    // Member 1 and member 2 touch the pile every few frames.
    let mut entries = Vec::new();
    let (mut p1, mut p2) = (EntryId::NONE, EntryId::NONE);
    for k in 0..12u32 {
        let e1 = touch(1, k + 1, p1, 5 + 8 * k as u64, k % 7, k % 2 == 0);
        let e2 = touch(2, k + 1, p2, 9 + 8 * k as u64, 7 + k % 5, false);
        p1 = e1.id();
        p2 = e2.id();
        entries.push(e1);
        entries.push(e2);
    }
    // Reference: never compacts; gets every entry before running.
    let mut a = sim(SnapPolicy::Every(4), CompactPolicy::Never);
    for e in &entries {
        a.insert(test_sign(1, e.clone()));
    }
    a.advance(110);
    // Compacts after every trim, on other snapshot policies; member 2's entries arrive late, in four
    // batches, so it rolls back between compactions.
    for snap in [SnapPolicy::Log(4), SnapPolicy::Window(4), SnapPolicy::Base(8)] {
        let mut b = sim(snap, CompactPolicy::EveryTrim);
        for e in entries.iter().filter(|e| e.member == 1) {
            b.insert(test_sign(1, e.clone()));
        }
        // A trim, and so a compaction, follows every advance: advance 5 frames at a time.
        let late: Vec<&Entry> = entries.iter().filter(|e| e.member == 2).collect();
        let mut batches = late.chunks(3);
        for t in (5..=110).step_by(5) {
            b.advance(t);
            if t % 25 == 0 {
                for e in batches.next().unwrap_or_default() {
                    b.insert(test_sign(1, (*e).clone()));
                }
            }
        }
        assert!(b.stats.compactions >= 20, "{snap:?}: compactions {}", b.stats.compactions);
        assert!(b.stats.rollbacks > 0, "{snap:?}: rolls back");
        for f in 0..110 {
            assert_eq!(a.hashes.get(&f), b.hashes.get(&f), "{snap:?}: state hash at frame {f}");
        }
        assert_eq!(a.check_hashes, b.check_hashes, "{snap:?}: check hashes");
        assert_eq!(a.check_hash(), b.check_hash(), "{snap:?}: live check hash");
    }
}
