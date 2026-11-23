//! Audio: enumerate output devices and play a single calibrated sine "pulse" to a chosen device.
//!
//! Shared-mode output: used by the `pulse` CLI, and by the engine as a fallback when exclusive
//! mode is refused. A pulse opens a short-lived stream on the target device, plays a sine (or sum
//! of sines) for `dur` seconds with raised-cosine fades to avoid clicks, then closes.

use std::f32::consts::PI;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

/// Format a cpal device for display and matching.
///
/// On Windows, `device.to_string()` is the unique WASAPI endpoint friendly name.
/// On Linux (ALSA), `device.to_string()` only prints the first line of the description (e.g.
/// `"USB Audio, USB Audio"`), which collides across `hw:`, `plughw:`, `front:`, `iec958:`, etc.
/// Including the ALSA PCM ID (`id().1` / `description().driver()`) makes each entry unambiguous
/// (e.g. `"hw:CARD=KA11,DEV=0 — KA11, USB Audio"`) while still matching raw PCM IDs like
/// `"hw:CARD=KA11,DEV=0"`.
pub(crate) fn format_device_name(device: &cpal::Device) -> String {
    #[cfg(windows)]
    {
        device.to_string()
    }
    #[cfg(not(windows))]
    {
        let label = device.to_string();
        if let Ok(id) = device.id() {
            let pcm_id = id.id();
            if !pcm_id.is_empty() && pcm_id != label {
                return format!("{pcm_id} ({label})");
            }
        }
        label
    }
}

/// Check whether `device` matches a user-supplied `want` string (exact display name, raw ALSA PCM
/// ID like `"hw:CARD=KA11,DEV=0"`, card name like `"hw:CARD=K2,DEV=0"` matching `hw:CARD=3,DEV=0`,
/// or card/description name).
pub(crate) fn device_matches(device: &cpal::Device, want: &str) -> bool {
    let want = want.trim();
    if format_device_name(device) == want || device.to_string() == want {
        return true;
    }
    if let Ok(id) = device.id() {
        let pcm_id = id.id();
        if pcm_id == want {
            return true;
        }
        #[cfg(target_os = "linux")]
        if alsa_pcm_id_matches(pcm_id, want) {
            return true;
        }
    }
    false
}

/// On Linux, cpal enumerates `hw:` and `plughw:` devices using numeric card indices (e.g.
/// `hw:CARD=3,DEV=0`), whereas `sysdefault:`/`front:` use the card ID string (e.g. `CARD=K2`) and
/// users often write `hw:CARD=K2,DEV=0` or `hw:3,0`. Resolve the card ID string to its numeric
/// index via ALSA so `hw:CARD=K2,DEV=0`, `hw:CARD=3,DEV=0`, and `hw:3,0` all match the same device.
#[cfg(target_os = "linux")]
fn alsa_pcm_id_matches(device_pcm_id: &str, want: &str) -> bool {
    let Some((d_prefix, d_card, d_dev)) = parse_alsa_hw_spec(device_pcm_id) else {
        return false;
    };
    // Strip any trailing description like "hw:CARD=3,DEV=0 (Komplete Audio 2, USB Audio)"
    let want_id = want.split_whitespace().next().unwrap_or(want);
    let Some((w_prefix, w_card, w_dev)) = parse_alsa_hw_spec(want_id) else {
        return false;
    };
    if d_prefix != w_prefix || d_dev != w_dev {
        return false;
    }
    if d_card == w_card {
        return true;
    }
    match (resolve_alsa_card_index(&d_card), resolve_alsa_card_index(&w_card)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[cfg(target_os = "linux")]
fn parse_alsa_hw_spec(spec: &str) -> Option<(&str, String, u32)> {
    let (prefix, rest) = spec.split_once(':')?;
    if !matches!(prefix, "hw" | "plughw" | "sysdefault" | "front" | "iec958" | "hdmi") {
        return None;
    }
    let mut card: Option<String> = None;
    let mut dev: u32 = 0;
    for (idx, part) in rest.split(',').enumerate() {
        let part = part.trim();
        if let Some(c) = part.strip_prefix("CARD=") {
            card = Some(c.to_string());
        } else if let Some(d) = part.strip_prefix("DEV=") {
            dev = d.parse().ok()?;
        } else if idx == 0 && !part.is_empty() {
            card = Some(part.to_string());
        } else if idx == 1 && !part.is_empty() {
            dev = part.parse().ok()?;
        }
    }
    Some((prefix, card?, dev))
}

#[cfg(target_os = "linux")]
pub(crate) fn resolve_alsa_card_index(card_str: &str) -> Option<i32> {
    if let Ok(idx) = card_str.parse::<i32>() {
        return Some(idx);
    }
    for card in alsa::card::Iter::new().filter_map(std::result::Result::ok) {
        let idx = card.get_index();
        if let Ok(ctl) = alsa::Ctl::new(&format!("hw:{idx}"), false) {
            if let Ok(info) = ctl.card_info() {
                if info.get_id().ok() == Some(card_str) || info.get_name().ok() == Some(card_str) {
                    return Some(idx);
                }
            }
        }
    }
    None
}

/// Names of all available output devices on the default host.
pub fn list_output_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let mut names = Vec::new();
    for device in host.output_devices()? {
        names.push(format_device_name(&device));
    }
    Ok(names)
}

/// Friendly name of the OS current default output device.
///
/// Used to warn when someone points the keep-awake pulse at the device they actually listen
/// through — the app manages that device's volume and format, which would wreck normal playback.
#[allow(dead_code)]
pub fn default_output_device_name() -> Result<String> {
    let host = cpal::default_host();
    host.default_output_device()
        .map(|d| format_device_name(&d))
        .ok_or_else(|| anyhow!("no default output device"))
}

/// (default shared-mode SR, max supported SR) in Hz. Max output frequency is SR/2.
pub fn device_sample_rate_info(name: Option<&str>) -> Result<(u32, u32)> {
    let device = resolve_output_device(name)?;
    let config: cpal::StreamConfig = device
        .default_output_config()
        .context("querying default output config")?
        .into();
    let default_sr = config.sample_rate as u32;
    let mut max_sr = default_sr;
    if let Ok(ranges) = device.supported_output_configs() {
        for r in ranges {
            let hi = r.max_sample_rate() as u32;
            // Ignore ALSA `plughw` synthetic u32::MAX (4294967295 Hz) if we can query the real `hw:` cap.
            if hi > max_sr {
                max_sr = hi;
            }
        }
    }
    #[cfg(target_os = "linux")]
    if max_sr > 768_000 {
        if let Some(s) = name {
            if let Some(rest) = s.trim().strip_prefix("plughw:") {
                let hw_name = format!("hw:{rest}");
                if let Ok((_, hw_max)) = device_sample_rate_info(Some(&hw_name)) {
                    if hw_max <= 768_000 {
                        max_sr = hw_max;
                    }
                }
            }
        }
    }
    Ok((default_sr, max_sr))
}

/// Convert a dBFS level (<= 0) to a linear 0..1 peak amplitude.
fn dbfs_to_amp(dbfs: f32) -> f32 {
    10f32.powf(dbfs / 20.0)
}

/// Refuse to synthesize a tone at or above Nyquist for the stream we actually got.
///
/// This matters because the locked pulse is **ultrasonic (30 kHz)** and inaudible only while the
/// device runs hi-res: shared mode inherits whatever rate Windows has set, so if the endpoint is
/// ever reset to 48 kHz, 30 kHz folds to an audible 18 kHz squeal with no other symptom. Failing
/// loudly beats silently emitting the alias into the room.
pub(crate) fn check_nyquist(freqs: &[f32], sample_rate: f32) -> Result<()> {
    let nyquist = sample_rate / 2.0;
    if let Some(f) = freqs.iter().copied().find(|f| *f >= nyquist) {
        bail!(
            "{f:.0} Hz is at/above Nyquist ({nyquist:.0} Hz) for this device's {sample_rate:.0} Hz \
             stream — it would alias to {:.0} Hz in the audible band. Set the endpoint back to a \
             hi-res format (check `device-info`) or lower the pulse frequency.",
            (sample_rate - f).abs()
        );
    }
    Ok(())
}

/// Sum one sample of `freqs` at frame index `n` for a stream running at `sr` Hz.
///
/// Phase is computed in **f64**. Doing this in f32 (as the original inline code did) loses the low
/// bits of the phase once `n` grows large: at 384 kHz a 5 s pulse reaches n ≈ 1.92 M, where the
/// f32 phase argument (~9.4e5 rad) has an ulp of ~0.06 rad against a per-sample step of 0.49 rad.
/// The resulting phase jitter smears the carrier into broadband hash — measured at −56 dB in the
/// audible band, which is exactly the "faint modem sound" heard on a 5 s 30 kHz pulse. f64 phase
/// puts the same artifact below −160 dB.
pub(crate) fn sine_sum(freqs: &[f32], n: usize, sr: f32) -> f32 {
    let sr = sr as f64;
    let n = n as f64;
    let mut s = 0.0f64;
    for &f in freqs {
        s += (2.0 * std::f64::consts::PI * f as f64 * n / sr).sin();
    }
    s as f32
}

/// Resolve an output device by exact name (or ALSA PCM ID), or the default output if `name` is None.
pub(crate) fn resolve_output_device(name: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    match name {
        None => host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device")),
        Some(want) => {
            for device in host.output_devices()? {
                if device_matches(&device, want) {
                    return Ok(device);
                }
            }
            bail!("output device not found: {want:?}")
        }
    }
}

/// Play one pulse of `freqs` summed (a single tone, or a chord) to `device_name`, or the default
/// device. Blocks for the pulse duration.
///
/// `dbfs` sets each component's amplitude; the sum is clamped to [-1, 1] so a chord can't clip.
/// `show_progress` draws an in-place progress bar, for single-shot CLI use.
pub fn play_pulse(
    device_name: Option<&str>,
    freqs: &[f32],
    dbfs: f32,
    dur_secs: f32,
    fade_ms: f32,
    decay_s: f32,
    show_progress: bool,
) -> Result<()> {
    let device = resolve_output_device(device_name)?;
    let supported = device
        .default_output_config()
        .context("querying default output config")?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    check_nyquist(freqs, config.sample_rate as f32)?;
    let amp = dbfs_to_amp(dbfs);
    let f = freqs.to_vec();

    match sample_format {
        SampleFormat::F32 => run::<f32>(&device, config, f, amp, dur_secs, fade_ms, decay_s, show_progress),
        SampleFormat::F64 => run::<f64>(&device, config, f, amp, dur_secs, fade_ms, decay_s, show_progress),
        SampleFormat::I32 => run::<i32>(&device, config, f, amp, dur_secs, fade_ms, decay_s, show_progress),
        SampleFormat::I24 => run::<cpal::I24>(&device, config, f, amp, dur_secs, fade_ms, decay_s, show_progress),
        SampleFormat::I16 => run::<i16>(&device, config, f, amp, dur_secs, fade_ms, decay_s, show_progress),
        SampleFormat::U16 => run::<u16>(&device, config, f, amp, dur_secs, fade_ms, decay_s, show_progress),
        other => bail!("unsupported sample format: {other:?}"),
    }
}

fn run<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    freqs: Vec<f32>,
    amp: f32,
    dur_secs: f32,
    fade_ms: f32,
    decay_s: f32,
    show_progress: bool,
) -> Result<()>
where
    T: SizedSample + FromSample<f32>,
{
    // cpal 0.18: sample_rate is a plain u32; build_output_stream takes config by value.
    let sr = config.sample_rate as f32;
    let channels = config.channels as usize;
    let total: usize = (dur_secs * sr) as usize;
    // fade length in samples, clamped so fades never overrun a short pulse
    let fade: usize = (((fade_ms / 1000.0) * sr) as usize)
        .min(total.max(2) / 2)
        .max(1);

    let mut n: usize = 0; // frame index, advanced once per frame (not per channel)
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            for frame in data.chunks_mut(channels) {
                let value = if n >= total {
                    0.0
                } else {
                    let mut s = sine_sum(&freqs, n, sr);
                    s *= amp;
                    if n < fade {
                        s *= 0.5 * (1.0 - (PI * n as f32 / fade as f32).cos());
                    } else if n > total - fade {
                        let j = (total - n) as f32;
                        s *= 0.5 * (1.0 - (PI * j / fade as f32).cos());
                    }
                    // Exponential decay (chime/bell character) when decay_s > 0.
                    if decay_s > 0.0 {
                        s *= (-(n as f32 / sr) / decay_s).exp();
                    }
                    s.clamp(-1.0, 1.0)
                };
                let sample = T::from_sample(value);
                for out in frame.iter_mut() {
                    *out = sample;
                }
                n += 1;
            }
        },
        |err| eprintln!("audio stream error: {err}"),
        None,
    )?;
    stream.play()?;
    // Hold the stream open for the pulse plus a small tail, then drop it (which stops playback).
    let play_secs = dur_secs + fade_ms / 1000.0;
    if show_progress {
        // Live progress bar (single-shot / calibration): shows exactly when it's playing.
        use std::io::Write;
        let step = 0.2f32;
        let steps = (play_secs / step).ceil().max(1.0) as u32;
        for i in 0..=steps {
            let elapsed = (i as f32 * step).min(play_secs);
            let filled = ((elapsed / play_secs) * 28.0) as usize;
            let bar: String = "#".repeat(filled) + &"-".repeat(28 - filled.min(28));
            print!("\r  \u{266a} PULSING [{bar}] {elapsed:>4.1}/{play_secs:.1}s ");
            let _ = std::io::stdout().flush();
            std::thread::sleep(Duration::from_secs_f32(step));
        }
        println!("\r  \u{266a} PULSE COMPLETE [{}] {play_secs:.1}s   ", "#".repeat(28));
    } else {
        std::thread::sleep(Duration::from_secs_f32(play_secs + 0.2));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the "faint modem sound" on long ultrasonic pulses (2026-07-24).
    ///
    /// Computing phase as `2*PI*f*(n as f32)/sr` loses precision as n grows: at the locked
    /// 30 kHz / 384 kHz / 5 s the phase reaches ~9.4e5 rad where an f32 ulp (~0.06 rad) is a
    /// large fraction of the 0.49 rad per-sample step. That jitter is audible hash. This test
    /// pins the late-stream samples against an exact f64 reference.
    #[test]
    fn sine_phase_stays_accurate_deep_into_a_long_ultrasonic_pulse() {
        let (f, sr) = (30_000.0f32, 384_000.0f32);
        // 5 s in: the point where the artifact was measured at -56 dB in the audible band.
        for n in [1_920_000usize, 1_920_001, 1_920_002, 2_500_000] {
            let got = sine_sum(&[f], n, sr);
            let want = (2.0 * std::f64::consts::PI * f as f64 * n as f64 / sr as f64).sin() as f32;
            assert!(
                (got - want).abs() < 1e-3,
                "n={n}: phase drift {:.4} (got {got:.6}, want {want:.6})",
                (got - want).abs()
            );
        }
    }

    #[test]
    fn sine_sum_adds_every_chord_component() {
        let s = sine_sum(&[100.0, 200.0], 480, 48_000.0);
        let want = ((2.0 * std::f64::consts::PI * 100.0 * 480.0 / 48_000.0).sin()
            + (2.0 * std::f64::consts::PI * 200.0 * 480.0 / 48_000.0).sin()) as f32;
        assert!((s - want).abs() < 1e-6, "got {s}, want {want}");
    }

    #[test]
    fn nyquist_allows_the_locked_ultrasonic_pulse_at_hi_res() {
        // KA11 shared-mode default: 384 kHz -> Nyquist 192 kHz, far above the locked 30 kHz.
        assert!(check_nyquist(&[30_000.0], 384_000.0).is_ok());
        assert!(check_nyquist(&[30_000.0], 96_000.0).is_ok());
    }

    #[test]
    fn nyquist_rejects_the_ultrasonic_pulse_if_the_endpoint_drops_to_48k() {
        // The real failure mode: Windows resets the device format and 30 kHz folds to 18 kHz.
        let err = check_nyquist(&[30_000.0], 48_000.0).unwrap_err().to_string();
        assert!(err.contains("Nyquist"), "unexpected message: {err}");
        assert!(err.contains("18000"), "should name the audible alias: {err}");
    }

    #[test]
    fn nyquist_rejects_exactly_at_nyquist_and_checks_every_chord_component() {
        assert!(check_nyquist(&[24_000.0], 48_000.0).is_err(), "f == Nyquist must fail");
        // A chord passes only if every component is below Nyquist.
        assert!(check_nyquist(&[50.0, 250.0, 500.0], 48_000.0).is_ok());
        assert!(check_nyquist(&[50.0, 250.0, 30_000.0], 48_000.0).is_err());
    }
}
