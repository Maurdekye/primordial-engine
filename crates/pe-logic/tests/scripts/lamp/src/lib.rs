#![no_std]
#![forbid(unsafe_code)]

pe_sdk::component! {
    /// How a lamp blinks.
    pub struct Blink { period: f32, phase: f32, near: f32 }
}

pe_sdk::component! {
    /// The lamp's light, this script's own stand-in for the engine's Light component.
    pub struct Glow { r: f32, g: f32, b: f32, intensity: f32 }
}

pe_sdk::system! {
    /// Blinks warm white on a period.
    fn pulse(stage: PrePhysics, read: [blink: Blink], write: [glow: Glow]) |ctx, me| {
        let t = ctx.time_s() + me.blink.phase;
        let on = t % me.blink.period < me.blink.period * 0.5;
        *me.glow = if on { Glow { r: 1.0, g: 0.8, b: 0.6, intensity: 1.0 } } else { Glow { r: 0.0, g: 0.0, b: 0.0, intensity: 0.0 } };
    }
}

pe_sdk::script!(components: [Blink, Glow], systems: [pulse]);
