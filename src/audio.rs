//! Voice capture and playback via cpal.
//!
//! The signal processing is split into two hardware-independent, unit-testable
//! pieces:
//!
//! * [`Framer`] — turns a device's interleaved PCM (any rate/channel count)
//!   into 48 kHz mono frames of [`FRAME_SAMPLES`] samples.
//! * [`Mixer`] — keeps one jitter buffer per remote speaker, loudness-normalises
//!   each source (basic RMS-target AGC), and mixes active speakers with equal
//!   weight (averaged over the active count) so simultaneous talkers stay level
//!   and never clip.
//!
//! [`Capture`]/[`Playback`] are thin cpal wrappers around those. Crucially they
//! honour the device's native **sample format** (I16/U16/F32), converting to and
//! from f32 — a mismatch here is the usual cause of "no audio".

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use crossbeam_channel::{Receiver, Sender};

use crate::protocol::SAMPLE_RATE;

/// 20 ms of mono audio at 48 kHz.
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize) / 50;

// ---- source normalisation (basic AGC) --------------------------------------
const TARGET_RMS: f32 = 0.12; // reference loudness we pull every source toward
const MAX_GAIN: f32 = 6.0; // never amplify more than this
const MIN_GAIN: f32 = 0.1;
const NOISE_GATE: f32 = 0.01; // below this a frame is treated as silence
const GAIN_SMOOTH: f32 = 0.15; // envelope follower speed (0..1)

/// Linear-interpolation resampler (good enough for speech).
pub(crate) fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 / ratio;
        let idx = src.floor() as usize;
        let frac = (src - idx as f64) as f32;
        let a = input[idx.min(input.len() - 1)];
        let b = input[(idx + 1).min(input.len() - 1)];
        out.push(a + (b - a) * frac);
    }
    out
}

/// Downmix interleaved frames to mono by averaging channels.
pub(crate) fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(channels)
        .map(|f| f.iter().copied().sum::<f32>() / channels as f32)
        .collect()
}

// ---- Framer (capture-side DSP, hardware independent) ------------------------

/// Converts interleaved device PCM into 48 kHz mono frames.
pub struct Framer {
    dev_rate: u32,
    channels: usize,
    pending: Vec<f32>,
}

impl Framer {
    pub fn new(dev_rate: u32, channels: usize) -> Self {
        Framer { dev_rate, channels, pending: Vec::with_capacity(FRAME_SAMPLES * 2) }
    }

    /// Feed one callback's worth of interleaved f32 samples; returns any whole
    /// 48 kHz mono frames that are now complete.
    pub fn push(&mut self, interleaved: &[f32]) -> Vec<Vec<f32>> {
        let mono = downmix(interleaved, self.channels);
        let resampled = resample_linear(&mono, self.dev_rate, SAMPLE_RATE);
        self.pending.extend_from_slice(&resampled);
        let mut out = Vec::new();
        while self.pending.len() >= FRAME_SAMPLES {
            out.push(self.pending.drain(..FRAME_SAMPLES).collect());
        }
        out
    }
}

// ---- Mixer (playback-side DSP, hardware independent) ------------------------

/// Number of consecutive silent renders before a speaker is forgotten. This is
/// deliberately generous so a source's AGC gain survives brief gaps between
/// packets instead of resetting every time its buffer momentarily drains.
const IDLE_LIMIT: u32 = 200;

/// Per-speaker playback state: a jitter buffer plus its smoothed AGC gain.
struct Source {
    buf: VecDeque<f32>,
    gain: f32,
    idle: u32,
}

impl Source {
    fn new() -> Self {
        Source { buf: VecDeque::new(), gain: 1.0, idle: 0 }
    }
}

/// Normalise a frame in place toward TARGET_RMS using a per-source smoothed gain.
fn normalize_in_place(samples: &mut [f32], gain: &mut f32) {
    if samples.is_empty() {
        return;
    }
    let rms = (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt();
    let desired = if rms < NOISE_GATE {
        1.0 // don't amplify silence/noise up to full scale
    } else {
        (TARGET_RMS / rms).clamp(MIN_GAIN, MAX_GAIN)
    };
    *gain = *gain * (1.0 - GAIN_SMOOTH) + desired * GAIN_SMOOTH;
    for s in samples.iter_mut() {
        *s = (*s * *gain).clamp(-1.0, 1.0);
    }
}

/// Equal-weight, per-source-normalised mixer targeting `rate` Hz.
pub struct Mixer {
    sources: HashMap<u64, Source>,
    rate: u32,
}

impl Mixer {
    pub fn new(rate: u32) -> Self {
        Mixer { sources: HashMap::new(), rate }
    }

    /// Enqueue a 48 kHz mono frame from remote speaker `peer_id`.
    pub fn push_frame(&mut self, peer_id: u64, pcm_48k_mono: &[f32]) {
        let mut samples = resample_linear(pcm_48k_mono, SAMPLE_RATE, self.rate);
        let src = self.sources.entry(peer_id).or_insert_with(Source::new);
        normalize_in_place(&mut samples, &mut src.gain);
        // Cap per-source latency: if we fell a second behind, resync.
        if src.buf.len() > self.rate as usize {
            src.buf.clear();
        }
        src.buf.extend(samples);
    }

    /// Number of speakers with audio still buffered.
    #[allow(dead_code)]
    pub fn active_sources(&self) -> usize {
        self.sources.values().filter(|s| !s.buf.is_empty()).count()
    }

    /// Render into an interleaved output buffer with `channels` channels,
    /// mixing active speakers with equal weight.
    pub fn render(&mut self, out: &mut [f32], channels: usize) {
        let channels = channels.max(1);
        // Age each source: those with data this render stay fresh; the rest
        // accumulate idle time toward eventual pruning (keeps AGC gain across
        // brief inter-packet gaps).
        for src in self.sources.values_mut() {
            if src.buf.is_empty() {
                src.idle = src.idle.saturating_add(1);
            } else {
                src.idle = 0;
            }
        }
        for frame in out.chunks_mut(channels) {
            let mut sum = 0.0f32;
            let mut active = 0u32;
            for src in self.sources.values_mut() {
                if let Some(s) = src.buf.pop_front() {
                    sum += s;
                    active += 1;
                }
            }
            let v = if active > 0 { sum / active as f32 } else { 0.0 };
            for o in frame.iter_mut() {
                *o = v;
            }
        }
        // Forget speakers that have been silent for a long time.
        self.sources.retain(|_, s| s.idle < IDLE_LIMIT);
    }
}

// ---- device enumeration -----------------------------------------------------

/// cpal 0.18 exposes the human-readable name via `Device::description()`.
fn device_name(d: &Device) -> Option<String> {
    d.description().ok().map(|desc| desc.name().to_string())
}

pub fn list_input_devices() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|it| it.filter_map(|d| device_name(&d)).collect())
        .unwrap_or_default()
}

pub fn list_output_devices() -> Vec<String> {
    cpal::default_host()
        .output_devices()
        .map(|it| it.filter_map(|d| device_name(&d)).collect())
        .unwrap_or_default()
}

fn find_input_device(name: &str) -> Option<Device> {
    cpal::default_host()
        .input_devices()
        .ok()?
        .find(|d| device_name(d).as_deref() == Some(name))
}

fn find_output_device(name: &str) -> Option<Device> {
    cpal::default_host()
        .output_devices()
        .ok()?
        .find(|d| device_name(d).as_deref() == Some(name))
}

fn resolve_input(name: &Option<String>) -> Result<Device, String> {
    match name {
        Some(n) => find_input_device(n).ok_or_else(|| format!("input device '{n}' not found")),
        None => cpal::default_host()
            .default_input_device()
            .ok_or_else(|| "no default input device".into()),
    }
}

fn resolve_output(name: &Option<String>) -> Result<Device, String> {
    match name {
        Some(n) => find_output_device(n).ok_or_else(|| format!("output device '{n}' not found")),
        None => cpal::default_host()
            .default_output_device()
            .ok_or_else(|| "no default output device".into()),
    }
}

// ---- capture ----------------------------------------------------------------

/// Microphone capture. Emits 48 kHz mono frames of length [`FRAME_SAMPLES`].
pub struct Capture {
    _stream: Stream,
}

/// Build an input stream for sample type `T`, converting each sample to f32.
fn build_input_stream<T>(
    device: &Device,
    config: &StreamConfig,
    mut framer: Framer,
    frames_out: Sender<Vec<f32>>,
) -> Result<Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let err_fn = |e| eprintln!("input stream error: {e}");
    device
        .build_input_stream(
            config.clone(),
            move |data: &[T], _| {
                let buf: Vec<f32> = data.iter().map(|s| f32::from_sample(*s)).collect();
                for frame in framer.push(&buf) {
                    let _ = frames_out.send(frame);
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| format!("build input stream: {e}"))
}

impl Capture {
    pub fn start(device: &Device, frames_out: Sender<Vec<f32>>) -> Result<Self, String> {
        let default = device
            .default_input_config()
            .map_err(|e| format!("default input config: {e}"))?;
        let channels = default.channels() as usize;
        let dev_rate = default.sample_rate();
        let fmt = default.sample_format();
        let config: StreamConfig = default.config();
        let framer = Framer::new(dev_rate, channels);

        let stream = match fmt {
            SampleFormat::F32 => build_input_stream::<f32>(device, &config, framer, frames_out)?,
            SampleFormat::I16 => build_input_stream::<i16>(device, &config, framer, frames_out)?,
            SampleFormat::U16 => build_input_stream::<u16>(device, &config, framer, frames_out)?,
            other => return Err(format!("unsupported input sample format: {other:?}")),
        };
        stream.play().map_err(|e| format!("play input: {e}"))?;
        Ok(Capture { _stream: stream })
    }
}

// ---- playback ---------------------------------------------------------------

type SharedMixer = Arc<Mutex<Mixer>>;

/// Speaker playback backed by a shared [`Mixer`].
pub struct Playback {
    _stream: Stream,
    mixer: SharedMixer,
    rate: u32,
}

/// Build an output stream for sample type `T`, converting f32 mix to `T`.
fn build_output_stream<T>(
    device: &Device,
    config: &StreamConfig,
    channels: usize,
    mixer: SharedMixer,
) -> Result<Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let err_fn = |e| eprintln!("output stream error: {e}");
    let mut scratch: Vec<f32> = Vec::new();
    device
        .build_output_stream(
            config.clone(),
            move |data: &mut [T], _| {
                scratch.clear();
                scratch.resize(data.len(), 0.0);
                mixer.lock().unwrap().render(&mut scratch, channels);
                for (o, s) in data.iter_mut().zip(scratch.iter()) {
                    *o = T::from_sample(*s);
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| format!("build output stream: {e}"))
}

impl Playback {
    pub fn start(device: &Device) -> Result<Self, String> {
        let default = device
            .default_output_config()
            .map_err(|e| format!("default output config: {e}"))?;
        let channels = default.channels() as usize;
        let rate = default.sample_rate();
        let fmt = default.sample_format();
        let config: StreamConfig = default.config();

        let mixer: SharedMixer = Arc::new(Mutex::new(Mixer::new(rate)));

        let stream = match fmt {
            SampleFormat::F32 => build_output_stream::<f32>(device, &config, channels, mixer.clone())?,
            SampleFormat::I16 => build_output_stream::<i16>(device, &config, channels, mixer.clone())?,
            SampleFormat::U16 => build_output_stream::<u16>(device, &config, channels, mixer.clone())?,
            other => return Err(format!("unsupported output sample format: {other:?}")),
        };
        stream.play().map_err(|e| format!("play output: {e}"))?;

        Ok(Playback { _stream: stream, mixer, rate })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Enqueue a 48 kHz mono frame from remote speaker `peer_id`.
    pub fn push_frame(&self, peer_id: u64, pcm_48k_mono: &[f32]) {
        self.mixer.lock().unwrap().push_frame(peer_id, pcm_48k_mono);
    }
}

// ---- high-level handle ------------------------------------------------------

/// Handle bundling capture + playback with runtime device selection.
pub struct Voice {
    capture: Option<Capture>,
    pub playback: Playback,
    pub out_rate: u32,
    input_name: Option<String>,  // None = system default
    output_name: Option<String>, // None = system default
    mic_on: bool,
    pub frames_rx: Receiver<Vec<f32>>,
    frames_tx: Sender<Vec<f32>>,
}

impl Voice {
    pub fn new() -> Result<Self, String> {
        let device = resolve_output(&None)?;
        let playback = Playback::start(&device)?;
        let out_rate = playback.rate();
        let (frames_tx, frames_rx) = crossbeam_channel::unbounded();
        Ok(Voice {
            capture: None,
            playback,
            out_rate,
            input_name: None,
            output_name: None,
            mic_on: false,
            frames_rx,
            frames_tx,
        })
    }

    pub fn input_name(&self) -> &Option<String> {
        &self.input_name
    }

    pub fn output_name(&self) -> &Option<String> {
        &self.output_name
    }

    fn start_capture(&mut self) -> Result<(), String> {
        let device = resolve_input(&self.input_name)?;
        self.capture = Some(Capture::start(&device, self.frames_tx.clone())?);
        Ok(())
    }

    /// Select the input (microphone) device. `None` means system default.
    pub fn set_input(&mut self, name: Option<String>) -> Result<(), String> {
        self.input_name = name;
        if self.mic_on {
            self.capture = None; // stop old stream first
            self.start_capture()?;
        }
        Ok(())
    }

    /// Select the output (speaker) device. `None` means system default.
    pub fn set_output(&mut self, name: Option<String>) -> Result<(), String> {
        let device = resolve_output(&name)?;
        let playback = Playback::start(&device)?;
        self.out_rate = playback.rate();
        self.playback = playback;
        self.output_name = name;
        Ok(())
    }

    pub fn set_mic(&mut self, on: bool) -> Result<(), String> {
        if on && self.capture.is_none() {
            self.start_capture()?;
        } else if !on {
            self.capture = None; // dropping stops the stream
        }
        self.mic_on = on;
        Ok(())
    }

    pub fn mic_on(&self) -> bool {
        self.mic_on
    }
}
