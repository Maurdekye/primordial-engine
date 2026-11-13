//! Systems for pe-logic's batch runner tests (`tests/run.rs`): order within a stage, the copy-in skip, batches,
//! creates, destroys, events, random streams and faults.
#![no_std]
#![forbid(unsafe_code)]

use pe_sdk::prelude::*;

pe_sdk::component! {
    /// Counts up by `step` every frame.
    pub struct Counter { n: u32, step: u32 }
}

pe_sdk::component! {
    /// Twice the counter, written by a later system of the same stage.
    pub struct Doubled { v: u64 }
}

pe_sdk::component! {
    /// Creates `left` more children, one a frame.
    pub struct Spawner { left: u32, made: u32 }
}

pe_sdk::component! {
    /// A spawned child: who made it and when. Reaped after two frames.
    pub struct Child { parent: ObjectId, born: u64 }
}

pe_sdk::component! {
    /// Two random draws a frame.
    pub struct Dice { a: u64, b: u64 }
}

pe_sdk::component! {
    /// Misbehaves on request: 1 writes NaN, 2 panics, 3 loops for ever, 4 recurses for ever, 5 destroys `victim`, 6
    /// recurses through linear memory.
    pub struct Trouble { trigger: u32, out: f32, victim: ObjectId }
}

pe_sdk::system! {
    fn count(stage: PrePhysics, read: [], write: [c: Counter]) |_ctx, me| {
        me.c.n = me.c.n.wrapping_add(me.c.step);
    }
}

pe_sdk::system! {
    fn double(stage: PrePhysics, read: [c: Counter], write: [d: Doubled]) |_ctx, me| {
        me.d.v = me.c.n as u64 * 2;
    }
}

pe_sdk::system! {
    fn spawn(stage: PrePhysics, read: [], write: [s: Spawner], creates: true, emits: ["spawned"]) |ctx, me| {
        if me.s.left > 0 {
            let child = Child { parent: ctx.me(), born: ctx.frame() };
            ctx.create(&child)?;
            ctx.emit(0, &me.s.made.to_le_bytes())?;
            me.s.left -= 1;
            me.s.made += 1;
        }
    }
}

pe_sdk::system! {
    fn reap(stage: PostPhysics, read: [c: Child], write: [], creates: true) |ctx, me| {
        if ctx.frame() >= me.c.born + 2 {
            let id = ctx.me();
            ctx.destroy(id)?;
        }
    }
}

pe_sdk::system! {
    fn roll(stage: PostPhysics, read: [], write: [d: Dice]) |ctx, me| {
        me.d.a = ctx.rand(0);
        me.d.b = ctx.rand(1);
    }
}

fn spin(n: u32) -> u32 {
    let mut x = n;
    loop {
        x = x.wrapping_mul(3).wrapping_add(1);
        if x == 0 {
            return x;
        }
    }
}

/// Recursion LLVM cannot turn into a loop (the result needs both the call and the argument), with no frame in linear
/// memory: only the depth meter stops it.
fn dive(n: u32) -> u32 {
    if n == 0 { 0 } else { dive(n.wrapping_add(1)).rotate_left(n) }
}

/// The same with each frame on rustc's shadow stack in linear memory (`black_box` takes the value's address): the
/// 64 KiB stack the build gives runs out first.
fn sink(n: u32) -> u32 {
    let m = core::hint::black_box(n);
    if m == 0 { 0 } else { sink(m.wrapping_add(1)).rotate_left(m) }
}

pe_sdk::system! {
    fn trouble(stage: PostPhysics, read: [], write: [t: Trouble], creates: true) |ctx, me| {
        match me.t.trigger {
            1 => me.t.out = f32::NAN,
            2 => panic!(),
            3 => me.t.out = spin(me.t.trigger) as f32,
            4 => me.t.out = dive(me.t.trigger) as f32,
            6 => me.t.out = sink(me.t.trigger) as f32,
            5 => {
                let v = me.t.victim;
                ctx.destroy(v)?;
            }
            _ => me.t.out += 1.0,
        }
    }
}

pe_sdk::component! {
    /// Declared read by `sneak`, which writes it anyway.
    pub struct Sneak { n: u32 }
}

pe_sdk::component! {
    /// What `peek` saw of Sneak.
    pub struct Peeked { v: u32 }
}

/// Declares Sneak as read, but opens the batch with a declaration of its own that writes it: safe code (no `unsafe`
/// anywhere), which the host must not trust (logic.md 3.3a (c); shader-reviewer M2).
pub mod sneak {
    use super::Sneak;
    use pe_sdk::component::Component;
    use pe_sdk::guest::{Batch, BatchArea, System};
    use pe_sdk::layout::{Access, Stage, status};
    use pe_sdk::manifest::SystemInfo;

    const DECLARED: SystemInfo = SystemInfo {
        name: "sneak",
        stage: Stage::PrePhysics,
        bound: &[(<Sneak as Component>::NAME, Access::Read)],
        reads_other: &[],
        emits: &[],
        creates: false,
    };
    const FORGED: SystemInfo = SystemInfo { bound: &[(<Sneak as Component>::NAME, Access::Write)], ..DECLARED };

    pub const SYSTEM: System = System { info: DECLARED, run };

    fn run(area: BatchArea<'_>) -> i32 {
        let forged = FORGED;
        let mut b = match Batch::new(area, &forged, &[<Sneak as Component>::SIZE]) {
            Ok(b) => b,
            Err(s) => return s,
        };
        match b.write::<Sneak>() {
            Ok(c) => c.iter_mut().for_each(|s| s.n = 999),
            Err(s) => return s,
        }
        b.finish();
        status::OK
    }
}

pe_sdk::system! {
    fn peek(stage: PrePhysics, read: [s: Sneak], write: [p: Peeked]) |_ctx, me| {
        me.p.v = me.s.n;
    }
}

pe_sdk::component! {
    /// 24-byte rows, which do not divide the write-back's 256-byte blocks: row 10 straddles the first boundary.
    pub struct Wide { flag: u64, b: u64, tail: u64 }
}

pe_sdk::system! {
    fn widen(stage: PostPhysics, read: [], write: [w: Wide]) |_ctx, me| {
        if me.w.flag != 0 {
            me.w.tail += 1;
        }
    }
}

pe_sdk::script!(
    components: [Counter, Doubled, Spawner, Child, Dice, Trouble, Sneak, Peeked, Wide],
    systems: [count, double, spawn, reap, roll, trouble, sneak, peek, widen],
);
