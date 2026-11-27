#![no_std]
#![forbid(unsafe_code)]
use pe_sdk::engine::Light;

pe_sdk::component! {
    /// How the lamp blinks: a full on/off cycle every `period` seconds, shifted by `phase`.
    pub struct Blink { period: f32, phase: f32 }
}

pe_sdk::system! {
    /// Switches the lamp's light on and off on its period.
    fn blink(stage: PrePhysics, read: [blink: Blink], write: [light: Light]) |ctx, me| {
        let t = ctx.time_s() + me.blink.phase;
        let on = t % me.blink.period < me.blink.period * 0.5;
        me.light.intensity = if on { 1.0 } else { 0.0 };
    }
}

pe_sdk::script!(components: [Blink], systems: [blink]);
