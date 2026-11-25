//! Player bodies (world-model.md 8.2; the friend test's locomotion, piece L3): each member's body as a pe-physics
//! player, walked once a frame by the mover before the hands are driven, and the view's rig and heads drawn from it.
//!
//! - **Like the hand bodies** ([`crate::hands`]), a player is the replica's own: the world model's body object has no
//!   `Body` row, and the replica keeps one pe-physics player under its id for each `Player` row
//!   ([`WorldModel::players`]), added at the body's feet when the row appears and removed when it goes ([`sync`]).
//!   Everything is a function of the rows and the physics state, applied in id order inside the step, and the
//!   player's state is in the physics image, so a rollback or a replay repeats it exactly.
//! - **The frame's move** ([`input`]): the row's stick, turned by its heading plus the stick's yaw into world axes by
//!   the fork's trig ([`walk_axes`]), times the mover's top walk speed; the room-scale step, the head's move over the
//!   floor since the last frame, turned by the heading; and the row's one-shot jump. The mover walks each body in id
//!   order, treating as absent every hand (players pass through each other, the user's 3a) and what the body's owner
//!   holds ([`ignored`]).
//! - **The rig** ([`rig`]) is where the tracking space stands so that the player's tracked head is over its feet: the
//!   feet less the head's offset over the floor, turned by the heading. It is drawing, never simulation, so it uses
//!   the standard library's trig; pe-app turns its hands into the body's frame with the same function
//!   ([`turn`]), so its hands and its view agree to the bit on one machine.

use pe_physics::{MoverInput, PLAYER, PhysicsWorld, PlayerParams, walk_axes};
use pe_seams::{HeadView, MemberKey, ObjectId, Offset, Pos, RigTransform};

use crate::world::{ClaimRow, HandRow, PlayerRow};

/// A room-scale step longer than this in one frame, m, is a discontinuity, never a head that walked (30 m/s): a
/// recentre, tracking regained, a glitch. The move then takes no room step, so the body stays and the view's rig, which
/// is recomputed from the row each frame, absorbs the jump (@design-reviewer). Above the longest step a
/// thinned or held Move leaves between two samples (about 0.2 m).
pub const ROOM_JUMP: f32 = 0.5;

/// Bring the players in line with the world model's player rows, before a frame's moves: a player for every row, at
/// its feet when it is new, and none for a body the rows no longer list. `rows` is [`WorldModel::players`], in id
/// order. Returns the number of refused physics calls (faults, replica.md 3.1).
///
/// [`WorldModel::players`]: crate::world::WorldModel::players
pub fn sync(physics: &mut PhysicsWorld, rows: &[PlayerRow], origin: Pos) -> u64 {
    let gone: Vec<ObjectId> = physics
        .objects()
        .filter(|&id| physics.is_player(id) && rows.binary_search_by_key(&id, |r| r.body).is_err())
        .collect();
    let mut faults = 0u64;
    for id in gone {
        faults += u64::from(physics.remove_body(id).is_err());
    }
    for r in rows {
        if !physics.has_body(r.body) {
            faults += u64::from(physics.add_player(r.body, (r.feet - origin).0, &PLAYER).is_err());
        }
    }
    faults
}

/// One frame's move for `row` (world-model.md 8.2): the stick in world axes at up to `params.max_walk`, the head's
/// room-scale step over the floor (none past [`ROOM_JUMP`]), both turned by the fork's trig, and the one-shot jump.
pub fn input(row: &PlayerRow, params: &PlayerParams) -> MoverInput {
    let axis = |v: i8| f32::from(v) / 127.0 * params.max_walk;
    let walk = walk_axes([axis(row.stick[0]), axis(row.stick[1])], row.heading.wrapping_add(row.stick_yaw));
    // The head's move over the floor in the rig frame, as (right, forward): forward is -Z. A jump past ROOM_JUMP is
    // no step at all.
    let (dx, dz) = (row.head_pos[0] - row.prev_head[0], row.head_pos[2] - row.prev_head[2]);
    let room = if dx * dx + dz * dz > ROOM_JUMP * ROOM_JUMP { [0.0, 0.0] } else { walk_axes([dx, -dz], row.heading) };
    MoverInput { walk, room, jump: row.jump }
}

/// What `row`'s move treats as absent: every hand (`hands`, the world model's), since players pass through each
/// other and a player's own hands are never in its way, and every object its owner holds (`claims`), so a held crate
/// never blocks or shoves its holder. In id order, once each.
pub fn ignored(row: &PlayerRow, hands: &[HandRow], claims: &[ClaimRow]) -> Vec<ObjectId> {
    let mut v: Vec<ObjectId> = hands.iter().map(|h| h.hand).collect();
    v.extend(claims.iter().filter(|c| c.member == row.owner).map(|c| c.object));
    v.sort_unstable();
    v.dedup();
    v
}

/// The cosine and sine of a heading (a wrapping angle, 2^32 one turn, positive turning left about +Y, 0 facing -Z):
/// drawing's trig, the standard library's, never the simulation's.
pub fn turn(heading: u32) -> (f64, f64) {
    let radians = f64::from(heading as i32) * (std::f64::consts::PI / 2_147_483_648.0);
    let (s, c) = radians.sin_cos();
    (c, s)
}

/// `v` turned by `heading` about +Y.
pub fn turned(heading: u32, v: [f64; 3]) -> [f64; 3] {
    let (c, s) = turn(heading);
    [c * v[0] + s * v[2], v[1], -s * v[0] + c * v[2]]
}

/// The heading's turn about +Y as a unit quaternion `[x, y, z, w]`.
pub fn heading_rotation(heading: u32) -> [f32; 4] {
    let half = f64::from(heading as i32) * (std::f64::consts::PI / 4_294_967_296.0);
    let (s, c) = half.sin_cos();
    [0.0, s as f32, 0.0, c as f32]
}

/// Where `row`'s tracking space stands (S5 v1.12's `local_rig`): its feet less the head's offset over the floor
/// turned by the heading, so the tracked head is over the feet, and turned by the heading. Scale 1. `None` only for a
/// position out of range.
pub fn rig(row: &PlayerRow) -> Option<RigTransform> {
    let over_floor = turned(row.heading, [f64::from(row.head_pos[0]), 0.0, f64::from(row.head_pos[2])]);
    let pos = row.feet.checked_add(Offset(over_floor.map(|c| -c))).ok()?;
    Some(RigTransform { pos, rotation: heading_rotation(row.heading), scale: 1.0 })
}

/// Every player's head at the view's frame (S5 v1.12), in member order: over its feet at the head's height, turned
/// by the heading, `own` on `local`'s.
pub fn heads(rows: &[PlayerRow], local: Option<MemberKey>) -> Vec<HeadView> {
    let mut v: Vec<HeadView> = rows
        .iter()
        .filter_map(|r| {
            let pos = r.feet.checked_add(Offset([0.0, f64::from(r.head_pos[1]), 0.0])).ok()?;
            let rotation = mul(heading_rotation(r.heading), r.head_rot);
            Some(HeadView { member: r.owner, body: r.body, pos, rotation, own: local == Some(r.owner) })
        })
        .collect();
    v.sort_by_key(|h| h.member);
    v
}

/// The Hamilton product `a * b` of `[x, y, z, w]` quaternions.
fn mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let [ax, ay, az, aw] = a;
    let [bx, by, bz, bw] = b;
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(heading: u32, head: [f32; 3]) -> PlayerRow {
        PlayerRow {
            body: ObjectId::from_u128(9),
            owner: MemberKey([1; 32]),
            feet: Pos::ORIGIN.checked_add(Offset([2.0, 0.5, -3.0])).unwrap(),
            stick: [0, 0],
            stick_yaw: 0,
            heading,
            jump: false,
            head_pos: head,
            head_rot: [0.0, 0.0, 0.0, 1.0],
            prev_head: head,
            tracked: true,
        }
    }

    fn near(a: [f64; 3], b: [f64; 3]) -> bool {
        a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-6)
    }

    /// The rig puts the tracked head over the feet whatever the heading: rig + R(heading) * head = feet + head height.
    #[test]
    fn the_rig_puts_the_tracked_head_over_the_feet() {
        for heading in [0u32, 1 << 30, 3 << 29, 0x1234_5678, u32::MAX] {
            let r = row(heading, [0.4, 1.6, -0.3]);
            let rig = rig(&r).expect("in range");
            let head = turned(heading, [0.4, 1.6, -0.3]);
            let world = rig.pos.checked_add(Offset(head)).unwrap();
            assert!(near((world - r.feet).0, [0.0, 1.6, 0.0]), "heading {heading:#x}: {:?}", (world - r.feet).0);
            let h = heads(&[r], None)[0];
            assert!(near((h.pos - r.feet).0, [0.0, 1.6, 0.0]));
        }
    }

    /// A quarter turn left (2^30) turns forward (-Z) to -X, as the fork's walk axes do.
    #[test]
    fn a_quarter_turn_left_faces_minus_x_as_the_mover_does() {
        let v = turned(1 << 30, [0.0, 0.0, -1.0]);
        assert!(near(v, [-1.0, 0.0, 0.0]), "{v:?}");
        let w = walk_axes([0.0, 1.0], 1 << 30);
        assert!((w[0] + 1.0).abs() < 1e-6 && w[1].abs() < 1e-6, "{w:?}");
        let q = heading_rotation(1 << 30);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        assert!((q[1] - h).abs() < 1e-6 && (q[3] - h).abs() < 1e-6, "{q:?}");
    }

    /// The room-scale step is the head's move over the floor, turned by the heading; the stick walks at up to the
    /// top speed toward the heading plus the stick's yaw.
    #[test]
    fn the_move_turns_the_stick_and_the_room_step_by_the_heading() {
        let mut r = row(1 << 30, [0.1, 1.6, 0.0]);
        r.prev_head = [0.0, 1.6, 0.0];
        r.stick = [0, 127];
        let m = input(&r, &PLAYER);
        assert!((m.walk[0] + PLAYER.max_walk).abs() < 1e-4 && m.walk[1].abs() < 1e-4, "{:?}", m.walk);
        // 0.1 m to the rig's right, turned a quarter left (facing -X, so right is -Z): world -Z.
        assert!(m.room[0].abs() < 1e-6 && (m.room[1] + 0.1).abs() < 1e-6, "{:?}", m.room);
        let t = turned(1 << 30, [0.1, 0.0, 0.0]);
        assert!(near(t, [0.0, 0.0, -0.1]), "the rig's turn agrees with the mover's: {t:?}");
        r.stick_yaw = 1 << 31; // the stick relative to a head turned half round: forward is now +X
        let m = input(&r, &PLAYER);
        assert!((m.walk[0] - PLAYER.max_walk).abs() < 1e-3, "{:?}", m.walk);
    }

    /// A head that jumps 2 m in a frame (a recentre) takes no room step; a 0.3 m step still goes.
    #[test]
    fn a_head_jump_past_room_jump_is_no_room_step() {
        let mut r = row(0, [2.0, 1.6, 0.0]);
        r.prev_head = [0.0, 1.6, 0.0];
        assert_eq!(input(&r, &PLAYER).room, [0.0, 0.0]);
        r.head_pos = [0.3, 1.6, 0.0];
        assert!((input(&r, &PLAYER).room[0] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn a_move_ignores_every_hand_and_what_its_owner_holds() {
        let r = row(0, [0.0, 1.6, 0.0]);
        let hand = |n: u128| HandRow {
            hand: ObjectId::from_u128(n),
            owner: MemberKey([2; 32]),
            target: Pos::ORIGIN,
            rotation: [0.0, 0.0, 0.0, 1.0],
            grip: 0.0,
            engaged: false,
            stamp: pe_seams::Frame(0),
            span: 0,
            previous: None,
            mode: 0,
            strength: 0.0,
        };
        let claim = |object: u128, member: MemberKey| ClaimRow {
            object: ObjectId::from_u128(object),
            hand: ObjectId::from_u128(3),
            member,
            since: pe_seams::Frame(0),
        };
        let ig = ignored(&r, &[hand(4), hand(3)], &[claim(7, r.owner), claim(5, MemberKey([2; 32]))]);
        assert_eq!(ig, [3, 4, 7].map(ObjectId::from_u128));
    }
}
