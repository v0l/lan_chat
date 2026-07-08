//! Voice capture and playback via cpal.
//!
//! Capture: native device config -> downmix to mono -> resample to 48 kHz ->
//! emit ~20 ms frames. Playback: accept 48 kHz mono frames -> resample to the
//! output device rate -> upmix to the output channel count, mixing concurrent
//! speakers together in a shared jitter buffer.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Stream, StreamConfig};
use crossbeam_channel::{Receiver, Sender};

use crate::protocol::SAMPLE_RATE;

/// 20 ms of mono audio at 48 kHz.
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize) / 50;

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

/// Microphone capture. Emits 48 kHz mono frames of length [`FRAME_SAMPLES`].
pub struct Capture {
    _stream: Stream,
}

impl Capture {
    pub fn start(frames_out: Sender<Vec<f32>>) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or("no default input device")?;
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

/// Speaker playback with a shared mixing jitter buffer of 48 kHz mono samples.
pub struct Playback {
    _stream: Stream,
    buffer: Arc<Mutex<VecDeque<f32>>>,
}

impl Playback {
    pub fn start() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("no default output device")?;
        let default = device
            .default_output_config()
            .map_err(|e| format!("default output config: {e}"))?;
        let channels = default.channels() as usize;
        let config: StreamConfig = default.config();

        // Buffer holds mono samples already resampled to the device rate.
        let buffer: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
        let buf_cb = buffer.clone();
        let err_fn = |e| eprintln!("output stream error: {e}");

        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    let mut buf = buf_cb.lock().unwrap();
                    for frame in data.chunks_mut(channels) {
                        let s = buf.pop_front().unwrap_or(0.0);
                        for out in frame.iter_mut() {
                            *out = s;
                        }
                    }
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("build output stream: {e}"))?;
        stream.play().map_err(|e| format!("play output: {e}"))?;

        Ok(Playback { _stream: stream, buffer })
    }

    /// Enqueue a 48 kHz mono frame from a remote speaker. Concurrent speakers
    /// are summed (mixed) where their buffered regions overlap.
    pub fn push_frame(&self, pcm_48k_mono: &[f32], dev_rate: u32) {
        let samples = resample_linear(pcm_48k_mono, SAMPLE_RATE, dev_rate);
        let mut buf = self.buffer.lock().unwrap();
        // Cap latency: if we're badly behind, drop the backlog.
        if buf.len() > dev_rate as usize {
            buf.clear();
        }
        for (i, s) in samples.iter().enumerate() {
            if let Some(existing) = buf.get_mut(i) {
                *existing = (*existing + *s).clamp(-1.0, 1.0);
            } else {
                buf.push_back(*s);
            }
        }
    }
}

/// Convenience: report the output device's native sample rate for `push_frame`.
pub fn output_device_rate() -> u32 {
    cpal::default_host()
        .default_output_device()
        .and_then(|d| d.default_output_config().ok())
        .map(|c| c.sample_rate().0)
        .unwrap_or(SAMPLE_RATE)
}

/// Handle bundling capture + playback so the GUI can toggle voice on/off.
pub struct Voice {
    pub capture: Option<Capture>,
    pub playback: Playback,
    pub out_rate: u32,
    pub frames_rx: Receiver<Vec<f32>>,
    frames_tx: Sender<Vec<f32>>,
}

impl Voice {
    pub fn new() -> Result<Self, String> {
        let playback = Playback::start()?;
        let out_rate = output_device_rate();
        let (frames_tx, frames_rx) = crossbeam_channel::unbounded();
        Ok(Voice { capture: None, playback, out_rate, frames_rx, frames_tx })
    }

    pub fn set_mic(&mut self, on: bool) -> Result<(), String> {
        if on && self.capture.is_none() {
            self.capture = Some(Capture::start(self.frames_tx.clone())?);
        } else if !on {
            self.capture = None; // dropping stops the stream
        }
        Ok(())
    }

    pub fn mic_on(&self) -> bool {
        self.capture.is_some()
    }
}
