//! End-to-end audio pipeline tests (no hardware required).
//!
//! These exercise the real DSP path used by the app:
//!   device PCM -> Framer (downmix + resample + framing)
//!             -> wire Envelope (encode/decode)
//!             -> Mixer (per-source normalise + equal-weight mix)
//!             -> interleaved output
//!
//! We can't open real audio devices in CI, so `Capture`/`Playback` (the thin
//! cpal wrappers) are not constructed here — but every sample-processing stage
//! they rely on is.

#[path = "../src/protocol.rs"]
mod protocol;
#[path = "../src/audio.rs"]
mod audio;

use audio::{Framer, Mixer, FRAME_SAMPLES};
use protocol::{Envelope, Payload, SAMPLE_RATE};

// ---- signal helpers ---------------------------------------------------------

/// Interleaved sine wave: `channels` identical channels, `secs` seconds.
fn sine(freq: f32, rate: u32, channels: usize, secs: f32, amp: f32) -> Vec<f32> {
    let n = (rate as f32 * secs) as usize;
    let mut out = Vec::with_capacity(n * channels);
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let s = (2.0 * std::f32::consts::PI * freq * t).sin() * amp;
        for _ in 0..channels {
            out.push(s);
        }
    }
    out
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Estimate the dominant frequency by counting zero crossings.
fn dominant_freq(samples: &[f32], rate: u32) -> f32 {
    let mut crossings = 0usize;
    for w in samples.windows(2) {
        if (w[0] <= 0.0 && w[1] > 0.0) || (w[0] >= 0.0 && w[1] < 0.0) {
            crossings += 1;
        }
    }
    let secs = samples.len() as f32 / rate as f32;
    (crossings as f32 / 2.0) / secs
}

// ---- tests ------------------------------------------------------------------

#[test]
fn framer_downmixes_resamples_and_frames() {
    // 100 ms of stereo 44.1 kHz -> expect 48 kHz mono frames.
    let dev_rate = 44_100;
    let input = sine(440.0, dev_rate, 2, 0.1, 0.5);

    let mut framer = Framer::new(dev_rate, 2);
    let frames = framer.push(&input);

    assert!(!frames.is_empty(), "framer produced no frames");
    // Every frame is exactly one 20 ms mono block.
    for f in &frames {
        assert_eq!(f.len(), FRAME_SAMPLES, "frame length must be FRAME_SAMPLES");
    }
    // ~100 ms at 48 kHz is ~4800 samples => ~5 frames of 960.
    assert!(
        (4..=6).contains(&frames.len()),
        "expected ~5 frames, got {}",
        frames.len()
    );
}

#[test]
fn full_pipeline_preserves_tone_through_the_wire() {
    // Capture a 440 Hz tone on a 44.1 kHz stereo "device", ship it over the
    // wire, mix it back on a 48 kHz "device", and confirm the tone survives.
    let dev_in = 44_100;
    let dev_out = SAMPLE_RATE; // 48 kHz output for a clean 1:1 mix
    let freq = 440.0;

    let input = sine(freq, dev_in, 2, 0.5, 0.4);
    let mut framer = Framer::new(dev_in, 2);
    let mut mixer = Mixer::new(dev_out);

    let mut rendered: Vec<f32> = Vec::new();
    for frame in framer.push(&input) {
        // --- wire round-trip: encode -> bytes -> decode ---
        let env = Envelope::new(
            7,
            Payload::Voice { name: "spk".into(), seq: 0, pcm: frame },
        );
        let bytes = env.encode();
        let decoded = Envelope::decode(&bytes).expect("decode envelope");
        let pcm = match decoded.payload {
            Payload::Voice { pcm, .. } => pcm,
            _ => panic!("expected Voice payload"),
        };

        mixer.push_frame(decoded.peer_id, &pcm);
        let mut out = vec![0.0f32; FRAME_SAMPLES]; // mono output
        mixer.render(&mut out, 1);
        rendered.extend_from_slice(&out);
    }

    // Ignore the AGC warm-up at the very start.
    let steady = &rendered[rendered.len() / 3..];
    assert!(rms(steady) > 0.02, "output is silent (rms={})", rms(steady));

    let f = dominant_freq(steady, dev_out);
    assert!(
        (f - freq).abs() < freq * 0.05,
        "recovered freq {f:.1} Hz not within 5% of {freq} Hz"
    );
}

#[test]
fn equal_weight_mixing_does_not_double_volume() {
    // Two speakers emitting the same tone should mix to roughly the same level
    // as one speaker (averaged), not twice as loud.
    let rate = SAMPLE_RATE;
    let tone = sine(300.0, rate, 1, 0.02, 0.3); // 20 ms mono frame-ish
    let tone = &tone[..FRAME_SAMPLES.min(tone.len())];

    let measure = |peers: &[u64]| -> f32 {
        let mut mixer = Mixer::new(rate);
        let mut tail: Vec<f32> = Vec::new();
        for i in 0..80 {
            for &p in peers {
                mixer.push_frame(p, tone);
            }
            let mut out = vec![0.0f32; FRAME_SAMPLES];
            mixer.render(&mut out, 1);
            if i >= 70 {
                tail.extend_from_slice(&out); // measure after AGC settles
            }
        }
        rms(&tail)
    };

    let one = measure(&[1]);
    let two = measure(&[1, 2]);

    assert!(one > 0.02, "single-speaker output too quiet: {one}");
    let ratio = two / one;
    assert!(
        (0.7..=1.3).contains(&ratio),
        "two equal speakers should stay ~1x one speaker, got {ratio:.2}x (equal-weight mixing broken)"
    );
}

#[test]
fn normalization_levels_quiet_and_loud_sources() {
    // A very quiet source is boosted and a hot source is tamed toward a common
    // target loudness by the per-source AGC.
    let rate = SAMPLE_RATE;

    let settled_rms = |amp: f32| -> f32 {
        let frame = sine(250.0, rate, 1, 0.02, amp);
        let frame = &frame[..FRAME_SAMPLES.min(frame.len())];
        let mut mixer = Mixer::new(rate);
        let mut tail: Vec<f32> = Vec::new();
        for i in 0..120 {
            mixer.push_frame(9, frame);
            let mut out = vec![0.0f32; FRAME_SAMPLES];
            mixer.render(&mut out, 1);
            if i >= 100 {
                tail.extend_from_slice(&out);
            }
        }
        rms(&tail)
    };

    let quiet_in = 0.03;
    let loud_in = 0.9;
    let quiet_out = settled_rms(quiet_in);
    let loud_out = settled_rms(loud_in);

    // The quiet source must be amplified above its raw level.
    assert!(
        quiet_out > quiet_in * 1.5,
        "quiet source not boosted: in={quiet_in}, out={quiet_out:.3}"
    );
    // The loud source must be attenuated below its raw level.
    assert!(
        loud_out < loud_in,
        "loud source not tamed: in={loud_in}, out={loud_out:.3}"
    );
    // Both should land in the same loudness ballpark.
    let ratio = quiet_out / loud_out;
    assert!(
        (0.5..=2.0).contains(&ratio),
        "normalised levels too far apart: quiet={quiet_out:.3}, loud={loud_out:.3}, ratio={ratio:.2}"
    );
}

#[test]
fn render_upmixes_mono_to_multichannel() {
    // A mono source rendered to a stereo device must fill both channels equally.
    let rate = SAMPLE_RATE;
    let frame = sine(400.0, rate, 1, 0.02, 0.5);
    let frame = &frame[..FRAME_SAMPLES.min(frame.len())];

    let mut mixer = Mixer::new(rate);
    // Push enough frames to clear the jitter buffer's prefill threshold so the
    // source is actually playing.
    for _ in 0..8 {
        mixer.push_frame(1, frame);
    }

    let mut out = vec![0.0f32; FRAME_SAMPLES * 2]; // stereo interleaved
    mixer.render(&mut out, 2);

    // Left and right of each frame must be identical.
    for pair in out.chunks(2) {
        assert_eq!(pair[0], pair[1], "stereo channels must match for mono source");
    }
    assert!(rms(&out) > 0.02, "stereo output silent");
}

#[test]
fn cue_is_audible_over_silence() {
    // A notification cue must produce sound even when no one is speaking.
    let rate = SAMPLE_RATE;
    let mut mixer = Mixer::new(rate);
    let cue = audio::synth_cue(audio::Cue::Join, rate);
    assert!(!cue.is_empty(), "cue synth produced nothing");
    mixer.push_sfx(&cue);
    let mut out = vec![0.0f32; cue.len()];
    mixer.render(&mut out, 1);
    assert!(rms(&out) > 0.02, "cue was not audible (rms={})", rms(&out));
}

#[test]
fn cue_mixes_on_top_of_voice() {
    // Cue + a speaker should both be present (cue bypasses AGC, adds on top).
    let rate = SAMPLE_RATE;
    let mut mixer = Mixer::new(rate);
    let tone = sine(300.0, rate, 1, 0.05, 0.2);
    mixer.push_frame(1, &tone[..FRAME_SAMPLES]);
    mixer.push_sfx(&audio::synth_cue(audio::Cue::Message, rate));
    let mut out = vec![0.0f32; FRAME_SAMPLES];
    mixer.render(&mut out, 1);
    assert!(rms(&out) > 0.02, "combined output silent");
}

#[test]
fn idle_mixer_outputs_silence() {
    let mut mixer = Mixer::new(SAMPLE_RATE);
    let mut out = vec![1.0f32; FRAME_SAMPLES];
    mixer.render(&mut out, 1);
    assert!(out.iter().all(|&s| s == 0.0), "idle mixer must render silence");
    assert_eq!(mixer.active_sources(), 0);
}
