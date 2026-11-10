//! The fixed-step tick (design 5).
//!
//! A [`Schedule`] is an ordered list of system *functions*. They are plain `fn` pointers, so a system
//! cannot keep state outside the store: anything that must survive a rollback lives in components. Each
//! tick runs:
//! 1. the floating-point environment check (4.3 rule 9);
//! 2. the frame's inputs, applied by the caller's closure (the session applies its entries here);
//! 3. every system, in order;
//! 4. the frame counter advances.
//!
//! Randomness comes from [`TickCtx::rng`]: a stream of `(session seed, stream id, frame)`, so systems need
//! no RNG state either.

use pe_core::Frame;
use pe_core::clock::TickRate;
use pe_core::fpenv;
use pe_core::rng::Rng;

use crate::store::SimStore;

/// What a system sees besides the store.
#[derive(Clone, Copy, Debug)]
pub struct TickCtx {
    /// The frame being stepped (the store holds the state at its start).
    pub frame: Frame,
    pub rate: TickRate,
    /// `rate.dt_f32()`.
    pub dt: f32,
    seed: u64,
}

impl TickCtx {
    /// The random stream `stream` for this frame.
    pub fn rng(&self, stream: u64) -> Rng {
        Rng::for_frame(self.seed, stream, self.frame.0)
    }
}

/// A system: a pure function of the store and the frame context.
pub type SystemFn = fn(&mut SimStore, &TickCtx);

#[derive(Clone, Debug)]
pub struct Schedule {
    rate: TickRate,
    systems: Vec<(&'static str, SystemFn)>,
    fp_resets: u64,
}

impl Schedule {
    pub fn new(rate: TickRate) -> Self {
        Schedule { rate, systems: Vec::new(), fp_resets: 0 }
    }

    pub fn with(mut self, name: &'static str, f: SystemFn) -> Self {
        self.systems.push((name, f));
        self
    }

    pub fn rate(&self) -> TickRate {
        self.rate
    }

    pub fn systems(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.systems.iter().map(|(n, _)| *n)
    }

    /// Ticks the floating-point environment had to be reset on (the caller should log any).
    pub fn fp_resets(&self) -> u64 {
        self.fp_resets
    }

    /// Steps one frame: `inputs` applies the frame's entries, then the systems run, then the frame advances.
    pub fn tick(&mut self, store: &mut SimStore, inputs: impl FnOnce(&mut SimStore, &TickCtx)) {
        if !fpenv::check_and_reset() {
            self.fp_resets += 1;
        }
        let ctx = TickCtx { frame: store.frame(), rate: self.rate, dt: self.rate.dt_f32(), seed: store.seed() };
        inputs(store, &ctx);
        for (_, f) in &self.systems {
            f(store, &ctx);
        }
        store.set_frame(ctx.frame.next());
    }

    /// Steps `n` frames with no inputs.
    pub fn run(&mut self, store: &mut SimStore, n: u64) {
        for _ in 0..n {
            self.tick(store, |_, _| {});
        }
    }
}
