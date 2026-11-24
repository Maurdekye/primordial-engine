//! Procedural impact voices and the fixed-block Steam Audio mix.

use std::f32::consts::TAU;
use std::path::Path;
use std::time::Instant;

use glam::{DVec3, Vec3 as Direction};
use pe_render::pose::{WorldPose, offset_vec};
use pe_seams::{Offset, Pos};

use super::phonon::{Orientation, Spatializer, Vec3};

pub(super) const RATE: u32 = 48_000;
pub(super) const BLOCK: usize = 128;
pub(super) const DIRECT: usize = 8;
pub(super) const MAX_VOICES: usize = 512;
const BED_CHANNELS: usize = 4;
const MAX_DISTANCE: f32 = 80.0;
const TONE_PERIOD: usize = 2048;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Material {
    Wood,
    Metal,
}

/// Local-only impact data at the audio boundary. The presentation adapter
/// decides when a hit is admitted, then supplies its position and character.
#[derive(Clone, Copy)]
pub(super) struct Impact {
    pub at: Pos,
    pub material: Material,
    pub size_m: f32,
    pub strength: f32,
    pub submitted: Instant,
}

struct Voice {
    id: u64,
    impact: Impact,
    gain: f32,
    pitch: f32,
    sample: f32,
    pending_onset: bool,
}

#[derive(Clone, Copy)]
struct Source {
    id: u64,
    index: usize,
    relative: Direction,
    distance_sq: f32,
    synthetic: bool,
}

pub(super) struct Mixer {
    sdk: Spatializer,
    voices: Vec<Voice>,
    sources: Vec<Source>,
    synthetic: Vec<Pos>,
    direct: [Option<u64>; DIRECT],
    next_id: u64,
    samples: usize,
    waveforms: [Vec<f32>; 2],
    tone: [f32; TONE_PERIOD],
    mono: [f32; BLOCK],
    wet: [f32; BLOCK * 2],
    encoded: [f32; BLOCK * BED_CHANNELS],
    bed: [f32; BLOCK * BED_CHANNELS],
    pub stereo: [f32; BLOCK * 2],
    pub onsets: Vec<Instant>,
    pub stolen: u64,
    pub rejected: u64,
    pub peak_voices: usize,
}

impl Mixer {
    pub fn new(path: &Path, synthetic_count: usize) -> Result<Self, String> {
        Ok(Self {
            sdk: Spatializer::open(path, RATE, BLOCK, DIRECT, MAX_VOICES - DIRECT, 1)?,
            voices: Vec::with_capacity(MAX_VOICES),
            sources: Vec::with_capacity(MAX_VOICES),
            synthetic: Vec::with_capacity(synthetic_count),
            direct: [None; DIRECT],
            next_id: 1,
            samples: 0,
            waveforms: [waveform(Material::Wood), waveform(Material::Metal)],
            tone: std::array::from_fn(|i| (TAU * i as f32 / TONE_PERIOD as f32).sin()),
            mono: [0.0; BLOCK],
            wet: [0.0; BLOCK * 2],
            encoded: [0.0; BLOCK * BED_CHANNELS],
            bed: [0.0; BLOCK * BED_CHANNELS],
            stereo: [0.0; BLOCK * 2],
            onsets: Vec::with_capacity(MAX_VOICES),
            stolen: 0,
            rejected: 0,
            peak_voices: 0,
        })
    }

    pub fn place_synthetic(&mut self, origin: Pos, count: usize) {
        for i in 0..count {
            let angle = i as f64 * 2.399_963_23;
            let radius = 1.5 + i as f64 * 0.035;
            let offset = Offset([radius * angle.sin(), 0.25 * (angle * 2.0).sin(), -radius * angle.cos()]);
            if let Ok(at) = origin.checked_add(offset) {
                self.synthetic.push(at);
            }
        }
    }

    pub fn admit(&mut self, impact: Impact, listener: Pos) {
        let gain = (0.028 * impact.strength.sqrt()).min(0.18);
        let pitch = impact.size_m.clamp(0.04, 4.0).powf(-0.32).clamp(0.5, 2.0);
        let distance = offset_vec(impact.at - listener).length() as f32;
        if !gain.is_finite() || gain <= 0.0 || !pitch.is_finite() || distance >= MAX_DISTANCE {
            self.rejected += 1;
            return;
        }
        let voice = Voice { id: self.next_id, impact, gain, pitch, sample: 0.0, pending_onset: true };
        self.next_id += 1;
        if self.voices.len() < MAX_VOICES {
            self.voices.push(voice);
            return;
        }
        let (quietest, priority) = self
            .voices
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let distance = offset_vec(v.impact.at - listener).length() as f32;
                (i, v.gain * attenuation(distance) / (1.0 + v.sample / 1024.0))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .expect("a full voice pool");
        if gain * attenuation(distance) > priority {
            self.voices[quietest] = voice;
            self.stolen += 1;
        } else {
            self.rejected += 1;
        }
    }

    pub fn active_voices(&self) -> usize {
        self.sources.len()
    }

    pub fn render(&mut self, listener: WorldPose) {
        self.stereo.fill(0.0);
        self.bed.fill(0.0);
        self.sources.clear();
        self.onsets.clear();
        for (index, voice) in self.voices.iter().enumerate() {
            self.sources.push(source(voice.id, index, voice.impact.at, listener.pos, false));
        }
        let synthetic = self.synthetic.len().saturating_sub(self.sources.len());
        for (i, &at) in self.synthetic.iter().take(synthetic).enumerate() {
            self.sources.push(source(u64::MAX - i as u64, i, at, listener.pos, true));
        }
        self.peak_voices = self.peak_voices.max(self.sources.len());
        self.sources.sort_unstable_by(|a, b| a.distance_sq.total_cmp(&b.distance_sq).then(a.id.cmp(&b.id)));
        let direct = &self.sources[..self.sources.len().min(DIRECT)];
        let last = self.direct;
        for slot in &mut self.direct {
            if !direct.iter().any(|source| Some(source.id) == *slot) {
                *slot = None;
            }
        }
        for source in direct {
            if !self.direct.contains(&Some(source.id)) {
                let slot = self.direct.iter_mut().find(|slot| slot.is_none()).expect("at most eight direct sources");
                *slot = Some(source.id);
            }
        }
        for (index, id) in self.direct.iter().enumerate() {
            if *id != last[index] {
                self.sdk.reset_binaural(index);
            }
        }
        let rotation = listener.rot.as_quat();
        for (index, source) in self.sources.iter().enumerate() {
            let direction = source.relative.try_normalize().unwrap_or(Direction::NEG_Z);
            let gain = attenuation(source.distance_sq.sqrt());
            if source.synthetic {
                let step = 5 + 23 * source.index % 40;
                let phase = self.samples.wrapping_mul(step) + source.index * 13;
                for (i, out) in self.mono.iter_mut().enumerate() {
                    *out = 0.003 * gain * self.tone[(phase + i * step) & (TONE_PERIOD - 1)];
                }
            } else {
                let voice = &mut self.voices[source.index];
                let wave = &self.waveforms[voice.impact.material as usize];
                for out in &mut self.mono {
                    let sample = voice.sample as usize;
                    let fraction = voice.sample.fract();
                    *out = wave.get(sample).map_or(0.0, |&a| {
                        let b = wave.get(sample + 1).copied().unwrap_or(0.0);
                        (a + fraction * (b - a)) * voice.gain * gain
                    });
                    voice.sample += voice.pitch;
                }
                if gain > 0.0 && std::mem::take(&mut voice.pending_onset) {
                    self.onsets.push(voice.impact.submitted);
                }
            }
            if index < DIRECT {
                let slot = self.direct.iter().position(|id| *id == Some(source.id)).expect("assigned above");
                let relative = rotation.inverse() * direction;
                self.sdk.binaural(slot, ipl(relative), false, &mut self.mono, &mut self.wet);
                add(&mut self.stereo, &self.wet);
            } else {
                self.sdk.encode(index - DIRECT, ipl(direction), &mut self.mono, &mut self.encoded);
                add(&mut self.bed, &self.encoded);
            }
        }
        self.voices.retain(|voice| (voice.sample as usize) < self.waveforms[voice.impact.material as usize].len());
        let orientation = Orientation {
            right: ipl(rotation * Direction::X),
            up: ipl(rotation * Direction::Y),
            ahead: ipl(rotation * Direction::NEG_Z),
            origin: Vec3::default(),
        };
        self.sdk.decode(orientation, &mut self.bed, &mut self.wet);
        add(&mut self.stereo, &self.wet);
        self.samples = (self.samples + BLOCK) & (TONE_PERIOD - 1);
    }
}

fn source(id: u64, index: usize, at: Pos, listener: Pos, synthetic: bool) -> Source {
    let relative = offset_vec(at - listener).clamp(DVec3::splat(-1e6), DVec3::splat(1e6)).as_vec3();
    Source { id, index, relative, distance_sq: relative.length_squared(), synthetic }
}

fn ipl(v: Direction) -> Vec3 {
    Vec3::new(v.x, v.y, v.z)
}

fn attenuation(distance: f32) -> f32 {
    (1.0 - distance / MAX_DISTANCE).max(0.0) / (1.0 + distance)
}

fn add(mix: &mut [f32], samples: &[f32]) {
    for (out, &sample) in mix.iter_mut().zip(samples) {
        *out += sample;
    }
}

fn waveform(material: Material) -> Vec<f32> {
    let (duration, modes) = match material {
        Material::Wood => (0.11, [(210.0, 0.55, 48.0), (430.0, 0.28, 75.0), (790.0, 0.17, 110.0)]),
        Material::Metal => (0.42, [(410.0, 0.46, 15.0), (1_031.0, 0.32, 24.0), (1_843.0, 0.22, 37.0)]),
    };
    let frames = (duration * RATE as f32) as usize;
    (0..frames)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            let attack = (t * 1750.0).min(1.0);
            let release = ((frames - i) as f32 / 240.0).min(1.0);
            attack * release * modes.iter().map(|&(f, a, d)| a * (TAU * f * t).sin() * (-d * t).exp()).sum::<f32>()
        })
        .collect()
}
