//! Systems for pe-logic's position tests (`tests/positions.rs`): pe-sdk's `Pos` arithmetic in wasm, cross-checked
//! by the host against pe-core's; `pe.Pose` and the `Transform` accessors over a real store.
#![no_std]
#![forbid(unsafe_code)]

use pe_sdk::engine::{Place, Pose, Transform};
use pe_sdk::prelude::*;

pe_sdk::component! {
    /// One case of the cross-check. In: `a`, `b`, `d`. Out: `sum = a + d` and `ok = 1`, or `ok = 0` when out of
    /// range; `diff = a - b`; `dist`, the distance from `a` to `b`.
    pub struct PosCase { a: Pos, b: Pos, sum: Pos, d: [f32; 3], diff: [f32; 3], dist: f32, ok: u32 }
}

pe_sdk::component! {
    /// What `locate` saw: the pose's position, the point `aim` ahead of it, whether the object is a root, and its
    /// place (the offset from its parent for a child, the difference from the pose's position for a root). `step` is
    /// the move `walk` makes each frame.
    pub struct Where {
        home: Pos,
        ahead: Pos,
        aim: OffsetFrom<Own>,
        root: u32,
        place: [f32; 3],
        step: OffsetFrom<Parent>,
    }
}

pe_sdk::component! {
    /// Moves `p` by `by` metres along x each frame with the trapping `+`.
    pub struct Far { p: Pos, by: f32, _pad: u32 }
}

pe_sdk::system! {
    fn cases(stage: PrePhysics, read: [], write: [c: PosCase]) |_ctx, me| {
        match me.c.a.checked_add(Offset::<World>::from_array(me.c.d)) {
            Ok(s) => {
                me.c.sum = s;
                me.c.ok = 1;
            }
            Err(OutOfRange) => {
                me.c.sum = ORIGIN;
                me.c.ok = 0;
            }
        }
        me.c.diff = (me.c.a - me.c.b).to_array();
        me.c.dist = me.c.a.distance(me.c.b);
    }
}

pe_sdk::system! {
    fn walk(stage: PrePhysics, read: [w: Where], write: [t: Transform]) |_ctx, me| {
        let _ = me.t.translate(me.w.step.get());
    }
}

pe_sdk::system! {
    fn locate(stage: PostPhysics, read: [pose: Pose, t: Transform], write: [w: Where]) |_ctx, me| {
        me.w.home = me.pose.pos();
        me.w.ahead = me.pose.at(me.w.aim.get()).unwrap_or(ORIGIN);
        me.w.root = me.pose.is_root() as u32;
        me.w.place = match me.t.place(me.pose) {
            Place::Root(p) => (p - me.pose.pos()).to_array(),
            Place::Child(o) => o.to_array(),
        };
    }
}

pe_sdk::system! {
    fn overflow(stage: PrePhysics, read: [], write: [f: Far]) |_ctx, me| {
        me.f.p = me.f.p + Offset::new(me.f.by, 0.0, 0.0);
    }
}

pe_sdk::script!(components: [PosCase, Where, Far, Pose, Transform], systems: [cases, walk, locate, overflow]);
