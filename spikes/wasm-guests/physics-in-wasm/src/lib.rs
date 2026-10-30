//! The physics spike's reference run (pile scene + scripted interactions), compiled to
//! wasm32-unknown-unknown and executed inside the host's interpreter. Returns the same digest as
//! `pe-spike-physics trace`, so native-vs-WASM determinism can be compared directly.

use pe_sim_core::*;

const SEED: u64 = 0x5052_494D_4F52_4449;

#[unsafe(no_mangle)]
pub extern "C" fn pe_run_trace(frames: u32) -> u64 {
    let inputs = scripted_inputs();
    let mut gs = GameState::new(build_pile_scene(8, 4), SEED);
    let mut state = Vec::with_capacity(frames as usize);
    let mut world = Vec::with_capacity(frames as usize);
    while gs.frame < frames as u64 {
        let f = gs.frame;
        let now: Vec<Interaction> = inputs.iter().copied().filter(|i| i.frame == f).collect();
        gs.advance(&now);
        state.push(gs.hash());
        world.push(gs.world_hash());
    }
    fnv_u64s(state.iter().copied().chain(world.iter().copied()))
}
