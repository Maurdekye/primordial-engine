//! Presented-world observer, Steam Audio output, and timing for the friend-test trial.

mod events;
pub(crate) mod headless;
mod mix;
mod phonon;

use std::error::Error;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use pe_render::counters::{Histogram, Summary};
use pe_render::pose::WorldPose;
use pe_render::present::Presenter;
use sha2::{Digest, Sha256};

use self::events::{Listener, Publisher};
use self::mix::{BLOCK, DEFAULT_VOICE_CAP, DIRECT, Impact, MAX_VOICES, Mixer, RATE, output_sample};

const RELAXED: Ordering = Ordering::Relaxed;
const WARMUP: Duration = Duration::from_secs(2);
const MAX_ADMITS_PER_BLOCK: usize = 64;
#[cfg(windows)]
const PHONON_SHA256: &str = "ca3dbc01dbc24492717011e80f6a51404ca143ae344ca660971d2c983f1e058d";
#[cfg(target_os = "linux")]
const PHONON_SHA256: &str = "faf7c4d69e0f104366f6553cda38d3f946f761f7db223baf12663f6fbf2387da";

#[derive(Clone)]
pub(crate) struct PublishAudio {
    listener: Listener,
    impacts: SyncSender<Impact>,
    fixture: bool,
    report: Arc<StreamCounters>,
}

impl PublishAudio {
    pub(crate) fn attach(&self, presenter: &mut Presenter) {
        Publisher {
            listener: self.listener.clone(),
            impacts: self.impacts.clone(),
            fixture: self.fixture,
            report: self.report.clone(),
        }
        .attach(presenter);
    }
}

pub(crate) struct AudioTrial {
    stream: Option<cpal::Stream>,
    publish: PublishAudio,
    metrics: Receiver<Report>,
    log: Option<File>,
}

impl AudioTrial {
    pub(crate) fn from_env() -> Option<Self> {
        let lib = std::env::var_os("PE_STEAM_AUDIO_LIB")?;
        let mut log = std::env::var_os("PE_STEAM_AUDIO_LOG").and_then(|path| match File::create(path) {
            Ok(file) => Some(file),
            Err(e) => {
                pe_app::err!("steam audio: cannot open the requested metrics log: {e}");
                None
            }
        });
        match Self::start(&PathBuf::from(lib), &mut log) {
            Ok(mut trial) => {
                trial.log = log;
                Some(trial)
            }
            Err(e) => {
                emit(&mut log, &format!("steam audio: silent fallback ({e})"));
                None
            }
        }
    }

    pub(crate) fn publisher(&self) -> PublishAudio {
        self.publish.clone()
    }

    fn start(path: &Path, log: &mut Option<File>) -> Result<Self, Box<dyn Error>> {
        let path = verified_library(path)?;
        let fixture = std::env::var("PE_STEAM_AUDIO_FIXTURE").ok();
        let synthetic_count = fixture.as_deref().unwrap_or("0").parse::<usize>()?;
        if synthetic_count > MAX_VOICES {
            return Err("PE_STEAM_AUDIO_FIXTURE must be between 0 and 512".into());
        }
        let voice_cap = DEFAULT_VOICE_CAP.max(synthetic_count);
        let audible = std::env::var_os("PE_STEAM_AUDIO_AUDIBLE").is_some_and(|s| s == "1");
        let host = cpal::default_host();
        let device = output_device(&host)?;
        let description = device.description()?;
        let range = device
            .supported_output_configs()?
            .filter(|range| {
                matches!(range.sample_format(), cpal::SampleFormat::F32 | cpal::SampleFormat::I16)
                    && range.min_sample_rate() <= RATE
                    && range.max_sample_rate() >= RATE
                    && range.channels() >= 2
            })
            .min_by_key(|range| (range.channels(), range.sample_format() != cpal::SampleFormat::F32))
            .ok_or("output device has no 48 kHz float or signed 16-bit configuration with two or more channels")?;
        let format = range.sample_format();
        let mut config = range.with_sample_rate(RATE).config();
        let channels = usize::from(config.channels);
        // The adapter handles endpoint callback sizes independently of the fixed DSP block.
        config.buffer_size = cpal::BufferSize::Default;
        let mix = Mixer::new(&path, voice_cap, synthetic_count)?;
        let (send_impact, recv_impact) = mpsc::sync_channel(MAX_VOICES);
        let (send_metrics, metrics) = mpsc::sync_channel(1);
        let publish = PublishAudio {
            listener: Listener::default(),
            impacts: send_impact,
            fixture: fixture.is_some(),
            report: Arc::default(),
        };
        let counters = publish.report.clone();
        let callback = Callback {
            mix,
            onsets: Vec::with_capacity(MAX_VOICES),
            clock: None,
            previous: None,
            listener: publish.listener.clone(),
            listener_pose: None,
            impacts: recv_impact,
            metrics: Some(Metrics::default()),
            send_metrics,
            synthetic_count,
            cursor: BLOCK,
            channels,
            audible,
        };
        let stream = match format {
            cpal::SampleFormat::F32 => output_stream::<f32>(&device, config.clone(), callback, counters)?,
            cpal::SampleFormat::I16 => output_stream::<i16>(&device, config.clone(), callback, counters)?,
            _ => unreachable!("filtered above"),
        };
        stream.play()?;
        emit(
            log,
            &format!(
                "steam audio: sdk=4.8.1 sha256={PHONON_SHA256} host={:?} endpoint={description:?} \
                 output={config:?} format={format:?} rate={RATE} block={BLOCK} direct={DIRECT} bed_order=1 hrtf=nearest \
                 voice_cap={voice_cap} fixture={fixture:?} audible={audible}",
                host.id()
            ),
        );
        Ok(Self { stream: Some(stream), publish, metrics, log: None })
    }
}

impl Drop for AudioTrial {
    fn drop(&mut self) {
        // The pinned WASAPI and ALSA backends join the output worker on drop.
        drop(self.stream.take());
        let Ok(report) = self.metrics.try_recv() else {
            emit(&mut self.log, "steam audio: callback metrics unavailable on shutdown");
            return;
        };
        let m = report.metrics;
        let counters = &self.publish.report;
        let received = counters.hits_received.load(RELAXED);
        let queued = counters.hits_queued.load(RELAXED);
        let full = counters.queue_drops.load(RELAXED);
        emit(
            &mut self.log,
            &format!(
                "steam audio: seconds={:.3} callbacks={} frames_min={} frames_max={} late_gaps={} callback_overruns={} \
                 errors={} reported_xruns={} pre_mute_peak={:.6} invalid_samples={} limited_samples={} \
                 hits_received={received} hits_queued={queued} queue_drops={full} gate_or_cap_drops={} \
                 voice_rejects={} voices_stolen={} peak_voices={} onset_samples_dropped={}",
                report.seconds,
                m.callbacks,
                if m.frames_max > 0 { m.frames_min } else { 0 },
                m.frames_max,
                m.late,
                m.overruns,
                counters.errors.load(RELAXED),
                counters.xruns.load(RELAXED),
                m.peak,
                m.invalid_samples,
                m.limited_samples,
                received.saturating_sub(queued + full),
                report.rejected,
                report.stolen,
                report.peak_voices,
                m.onset_samples_dropped,
            ),
        );
        for (name, histogram) in [
            ("dsp_block_us", m.dsp),
            ("dsp_block_8plus_voices_us", m.busy_dsp),
            ("active_voices_per_block", m.active_voices),
            ("callback_jitter_us", m.jitter),
            ("callback_work_us", m.callback),
            ("hit_to_carrying_callback_us", m.onset),
            ("backend_playback_estimate_us", m.backend),
        ] {
            let Summary { count, p50, p99, max, .. } = histogram.summary();
            emit(&mut self.log, &format!("steam audio: {name} n={count} p50={p50:.3} p99={p99:.3} max={max:.3}"));
        }
    }
}

#[derive(Default)]
struct StreamCounters {
    errors: AtomicU64,
    xruns: AtomicU64,
    hits_received: AtomicU64,
    hits_queued: AtomicU64,
    queue_drops: AtomicU64,
}

#[derive(Default)]
struct Metrics {
    dsp: Histogram,
    busy_dsp: Histogram,
    active_voices: Histogram,
    jitter: Histogram,
    callback: Histogram,
    onset: Histogram,
    backend: Histogram,
    callbacks: u64,
    frames_min: usize,
    frames_max: usize,
    late: u64,
    overruns: u64,
    peak: f32,
    invalid_samples: u64,
    limited_samples: u64,
    onset_samples_dropped: u64,
}

struct Report {
    metrics: Metrics,
    seconds: f64,
    rejected: u64,
    stolen: u64,
    peak_voices: usize,
}

struct Callback {
    mix: Mixer,
    onsets: Vec<Instant>,
    clock: Option<Instant>,
    previous: Option<(Instant, usize)>,
    listener: Listener,
    listener_pose: Option<WorldPose>,
    impacts: Receiver<Impact>,
    metrics: Option<Metrics>,
    send_metrics: SyncSender<Report>,
    synthetic_count: usize,
    cursor: usize,
    channels: usize,
    audible: bool,
}

impl Callback {
    fn fill<T: cpal::Sample + cpal::FromSample<f32>>(&mut self, output: &mut [T], info: &cpal::OutputCallbackInfo) {
        let start = Instant::now();
        output.fill(T::EQUILIBRIUM);
        if let Ok(listener) = self.listener.try_lock() {
            self.listener_pose = *listener;
        }
        let Some(listener) = self.listener_pose else { return };
        let frames = output.len() / self.channels;
        if frames == 0 {
            return;
        }
        if self.clock.is_none() {
            self.mix.place_synthetic(listener.pos, self.synthetic_count);
        }
        let recording = start.duration_since(*self.clock.get_or_insert(start)) >= WARMUP;
        let stats = self.metrics.as_mut().expect("callback still owns its metrics");
        if recording {
            stats.callbacks += 1;
            if stats.frames_max == 0 {
                stats.frames_min = frames;
            }
            stats.frames_min = stats.frames_min.min(frames);
            stats.frames_max = stats.frames_max.max(frames);
            if let Some((previous, previous_frames)) = self.previous {
                let interval = start.duration_since(previous);
                let expected = frame_period(previous_frames);
                stats.jitter.record(micros(interval.abs_diff(expected)));
                stats.late += u64::from(interval > 2 * expected);
            }
            let stamp = info.timestamp();
            stats.backend.record(micros(stamp.playback.duration_since(stamp.callback)));
        }
        self.previous = Some((start, frames));
        self.onsets.clear();
        for frame in output.chunks_exact_mut(self.channels) {
            if self.cursor == BLOCK {
                let dsp_start = Instant::now();
                for hit in self.impacts.try_iter().take(MAX_ADMITS_PER_BLOCK) {
                    self.mix.admit(hit, listener.pos);
                }
                self.mix.render(listener);
                if recording {
                    let dsp_us = micros(dsp_start.elapsed());
                    let active_voices = self.mix.active_voices();
                    stats.dsp.record(dsp_us);
                    stats.active_voices.record(active_voices as f64);
                    if active_voices >= DIRECT {
                        stats.busy_dsp.record(dsp_us);
                    }
                    let room = self.onsets.capacity() - self.onsets.len();
                    stats.onset_samples_dropped += self.mix.onsets.len().saturating_sub(room) as u64;
                    self.onsets.extend(self.mix.onsets.iter().take(room).copied());
                }
                self.cursor = 0;
            }
            for (channel, out) in frame.iter_mut().take(2).enumerate() {
                let value = self.mix.stereo[self.cursor + BLOCK * channel];
                if !value.is_finite() {
                    stats.invalid_samples += u64::from(recording);
                    continue;
                }
                let (sample, limited) = output_sample(value);
                if recording {
                    stats.peak = stats.peak.max(sample.abs());
                    stats.limited_samples += u64::from(limited);
                }
                if self.audible {
                    *out = T::from_sample(sample);
                }
            }
            self.cursor += 1;
        }
        if recording {
            let finished = Instant::now();
            let work = finished.duration_since(start);
            stats.callback.record(micros(work));
            stats.overruns += u64::from(work > frame_period(frames));
            for hit in &self.onsets {
                stats.onset.record(micros(finished.duration_since(*hit)));
            }
        }
    }
}

impl Drop for Callback {
    fn drop(&mut self) {
        if let Some(metrics) = self.metrics.take() {
            let _ = self.send_metrics.try_send(Report {
                metrics,
                seconds: self.clock.map_or(0.0, |t| t.elapsed().saturating_sub(WARMUP).as_secs_f64()),
                rejected: self.mix.rejected,
                stolen: self.mix.stolen,
                peak_voices: self.mix.peak_voices,
            });
        }
    }
}

fn output_device(host: &cpal::Host) -> Result<cpal::Device, Box<dyn Error>> {
    if let Ok(name) = std::env::var("PE_STEAM_AUDIO_DEVICE") {
        for device in host.output_devices()? {
            if device.description().is_ok_and(|d| d.name() == name) {
                return Ok(device);
            }
        }
        return Err(format!("no output device matches {name:?}").into());
    }
    host.default_output_device().ok_or_else(|| "no default audio output device".into())
}

fn output_stream<T: cpal::SizedSample + cpal::FromSample<f32>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut callback: Callback,
    counters: Arc<StreamCounters>,
) -> Result<cpal::Stream, cpal::Error> {
    device.build_output_stream::<T, _, _>(
        config,
        move |buffer, info| callback.fill(buffer, info),
        move |error| {
            counters.errors.fetch_add(1, RELAXED);
            if error.kind() == cpal::ErrorKind::Xrun {
                counters.xruns.fetch_add(1, RELAXED);
            }
        },
        None,
    )
}

fn verified_library(path: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let path = path.canonicalize()?;
    let mut file = File::open(&path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    let digest = format!("{:x}", hash.finalize());
    if digest != PHONON_SHA256 {
        return Err(format!("{} does not match the pinned v4.8.1 binary (SHA-256 {digest})", path.display()).into());
    }
    Ok(path)
}

fn emit(log: &mut Option<File>, line: &str) {
    pe_app::out!("{line}");
    if let Some(file) = log
        && let Err(e) = writeln!(file, "{line}")
    {
        pe_app::err!("steam audio: metrics log failed: {e}");
        *log = None;
    }
}

fn frame_period(frames: usize) -> Duration {
    Duration::from_nanos(frames as u64 * 1_000_000_000 / u64::from(RATE))
}

fn micros(t: Duration) -> f64 {
    t.as_secs_f64() * 1e6
}
