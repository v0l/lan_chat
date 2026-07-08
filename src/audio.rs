//! Voice capture and playback via cpal.
//!
//! Capture: chosen device's native config -> downmix to mono -> resample to
//! 48 kHz -> emit ~20 ms frames.
//!
//! Playback: keeps one jitter buffer **per remote speaker**. Each incoming
//! source is loudness-normalised (basic RMS-targeting AGC) so quiet and loud
//! mics come out at a comparable level, then the output callback mixes all
//! currently-active speakers with **equal weight** by averaging over the number
//! of active sources — so N people talking at once stays at a sane volume
//! instead of summing into clipping.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Stream, StreamConfig};
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
fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
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
fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(channels)
        .map(|f| f.iter().copied().sum::<f32>() / channels as f32)
        .collect()
}

// ---- device enumeration -----------------------------------------------------

pub fn list_input_devices() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

pub fn list_output_devices() -> Vec<String> {
    cpal::default_host()
        .output_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

fn find_input_device(name: &str) -> Option<Device> {
    cpal::default_host()
        .input_devices()
        .ok()?
        .find(|d| d.name().ok().as_deref() == Some(name))
}

fn find_output_device(name: &str) -> Option<Device> {
    cpal::default_host()
        .output_devices()
        .ok()?
        .find(|d| d.name().ok().as_deref() == Some(name))
}

/// Resolve an optional device name to a concrete input device (None = default).
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

impl Capture {
    pub fn start(device: &Device, frames_out: Sender<Vec<f32>>) -> Result<Self, String> {
        let default = device
            .default_input_config()
            .map_err(|e| format!("default input config: {e}"))?;
        let channels = default.channels() as usize;
        let dev_rate = default.sample_rate().0;
        let config: StreamConfig = default.config();

        let err_fn = |e| eprintln!("input stream error: {e}");
        let mut pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 2);

        let stream = device
            .build_input_stream(
                &config,
                move |data: &[f32], _| {
                    let mono = downmix(data, channels);
                    let resampled = resample_linear(&mono, dev_rate, SAMPLE_RATE);
                    pending.extend_from_slice(&resampled);
                    while pending.len() >= FRAME_SAMPLES {
                        let frame: Vec<f32> = pending.drain(..FRAME_SAMPLES).collect();
                        let _ = frames_out.send(frame);
                    }
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("build input stream: {e}"))?;
        stream.play().map_err(|e| format!("play input: {e}"))?;
        Ok(Capture { _stream: stream })
    }
}

// ---- playback / mixing ------------------------------------------------------

/// Per-speaker playback state: a jitter buffer plus its smoothed AGC gain.
struct Source {
    buf: VecDeque<f32>,
    gain: f32,
}

impl Source {
    fn new() -> Self {
        Source { buf: VecDeque::new(), gain: 1.0 }
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

type SourceMap = Arc<Mutex<HashMap<u64, Source>>>;

/// Speaker playback with equal-weight mixing across active speakers.
pub struct Playback {
    _stream: Stream,
    sources: SourceMap,
    rate: u32,
}

impl Playback {
    pub fn start(device: &Device) -> Result<Self, String> {
        let default = device
            .default_output_config()
            .map_err(|e| format!("default output config: {e}"))?;
        let channels = default.channels() as usize;
        let rate = default.sample_rate().0;
        let config: StreamConfig = default.config();

        let sources: SourceMap = Arc::new(Mutex::new(HashMap::new()));
        let sources_cb = sources.clone();
        let err_fn = |e| eprintln!("output stream error: {e}");

        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    let mut map = sources_cb.lock().unwrap();
                    for frame in data.chunks_mut(channels) {
                        // Equal-weight mix: sum one sample from each active
                        // speaker, then divide by the number that were active.
                        let mut sum = 0.0f32;
                        let mut active = 0u32;
                        for src in map.values_mut() {
                            if let Some(s) = src.buf.pop_front() {
                                sum += s;
                                active += 1;
                            }
                        }
                        let v = if active > 0 { sum / active as f32 } else { 0.0 };
                        for out in frame.iter_mut() {
                            *out = v;
                        }
                    }
                    // Drop drained speakers so the map doesn't grow forever.
                    map.retain(|_, s| !s.buf.is_empty());
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("build output stream: {e}"))?;
        stream.play().map_err(|e| format!("play output: {e}"))?;

        Ok(Playback { _stream: stream, sources, rate })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Enqueue a 48 kHz mono frame from remote speaker `peer_id`. The source is
    /// loudness-normalised, then it participates in equal-weight mixing.
    pub fn push_frame(&self, peer_id: u64, pcm_48k_mono: &[f32], dev_rate: u32) {
        let mut samples = resample_linear(pcm_48k_mono, SAMPLE_RATE, dev_rate);
        let mut map = self.sources.lock().unwrap();
        let src = map.entry(peer_id).or_insert_with(Source::new);
        normalize_in_place(&mut samples, &mut src.gain);
        // Cap per-source latency: if we fell a second behind, resync.
        if src.buf.len() > dev_rate as usize {
            src.buf.clear();
        }
        src.buf.extend(samples);
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
