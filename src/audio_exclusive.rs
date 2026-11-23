//! Exclusive-mode pulse output (Windows: WASAPI exclusive; Linux: direct ALSA PCM / cpal stream
//! at the requested sample rate).
//!
//! Shared-mode audio on Windows resamples everything to the device's 48 kHz mix format, capping
//! output at 24 kHz. Exclusive mode opens the dongle directly at a requested sample rate (e.g.
//! 96 kHz / 384 kHz), so we can emit tones above 24 kHz — needed for the >25 kHz "above the
//! speakers' reproduction" band. On Linux, direct ALSA hardware (`hw:`/`plughw:`) or PipeWire/ALSA
//! negotiates the requested rate directly on the stream.
//!
//! Safety: a wall-clock watchdog bounds the render loop, and the audio client/stream releases the
//! device when it drops (even on error/panic), so a stuck pulse can never permanently hold the dongle.

use std::f32::consts::PI;
#[cfg(windows)]
use std::time::Instant;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

fn dbfs_to_amp(dbfs: f32) -> f32 {
    10f32.powf(dbfs / 20.0)
}

// ============================================================================
// Windows implementation (WASAPI exclusive)
// ============================================================================
#[cfg(windows)]
mod imp {
    use super::*;
    use wasapi::*;

    /// Resolve a render device by exact friendly name.
    fn find_device(name: &str) -> Result<Device> {
        let enumerator = DeviceEnumerator::new().map_err(|e| anyhow!("device enumerator: {e:?}"))?;
        for device in &enumerator
            .get_device_collection(&Direction::Render)
            .map_err(|e| anyhow!("device collection: {e:?}"))?
        {
            let dev = device.map_err(|e| anyhow!("device: {e:?}"))?;
            if dev
                .get_friendlyname()
                .map_err(|e| anyhow!("friendlyname: {e:?}"))?
                == name
            {
                return Ok(dev);
            }
        }
        bail!("output device not found: {name:?}")
    }

    /// Convert a normalized sample (-1.0..1.0) to little-endian bytes for the device's format.
    fn pack_sample(v: f32, stype: &SampleType, bits: u16, out: &mut [u8]) {
        let v = v.clamp(-1.0, 1.0);
        match (stype, bits) {
            (SampleType::Float, 32) => out.copy_from_slice(&v.to_le_bytes()),
            (SampleType::Int, 16) => {
                let i = (v * 32767.0) as i16;
                out.copy_from_slice(&i.to_le_bytes());
            }
            (SampleType::Int, 24) => {
                let i = (v * 8_388_607.0) as i32;
                let b = i.to_le_bytes();
                out.copy_from_slice(&b[0..3]);
            }
            (SampleType::Int, 32) => {
                let i = (v * 2_147_483_647.0) as i32;
                out.copy_from_slice(&i.to_le_bytes());
            }
            _ => {
                for b in out.iter_mut() {
                    *b = 0;
                }
            }
        }
    }

    /// Probe which sample rates the device accepts in EXCLUSIVE mode (and the first format that works).
    /// Returns one line per rate. Does not play anything.
    pub fn probe_exclusive_support(device_name: &str, rates: &[u32]) -> Result<Vec<String>> {
        // COM may already be initialized (e.g. cpal set STA earlier in this process); either is fine.
        let _ = initialize_mta();
        let device = find_device(device_name)?;
        let attempts: [(SampleType, usize); 4] = [
            (SampleType::Float, 32),
            (SampleType::Int, 32),
            (SampleType::Int, 24),
            (SampleType::Int, 16),
        ];
        let mut out = Vec::new();
        for &rate in rates {
            let audio_client = match device.get_iaudioclient() {
                Ok(c) => c,
                Err(e) => {
                    out.push(format!("{rate:>7} Hz: client error {e:?}"));
                    continue;
                }
            };
            let mut supported = None;
            for (st, bits) in attempts.iter() {
                let desired = WaveFormat::new(*bits, *bits, st, rate as usize, 2, None);
                if audio_client.is_supported_exclusive_with_quirks(&desired).is_ok() {
                    supported = Some(format!("{st:?}{bits}"));
                    break;
                }
            }
            match supported {
                Some(fmt) => out.push(format!("{rate:>7} Hz: OK ({fmt})   max output {} Hz", rate / 2)),
                None => out.push(format!("{rate:>7} Hz: not supported (exclusive)")),
            }
        }
        Ok(out)
    }

    /// Play one pulse of `freqs` summed, in exclusive mode at `sample_rate`, for `dur_secs`.
    pub fn play_pulse_exclusive(
        device_name: &str,
        freqs: &[f32],
        dbfs: f32,
        dur_secs: f32,
        sample_rate: u32,
        fade_ms: f32,
    ) -> Result<()> {
        crate::audio::check_nyquist(freqs, sample_rate as f32)?;
        let _ = initialize_mta();
        let device = find_device(device_name)?;
        let mut audio_client = device
            .get_iaudioclient()
            .map_err(|e| anyhow!("get_iaudioclient: {e:?}"))?;

        let channels = 2usize;
        // Try formats in order of preference; exclusive mode often only accepts integer PCM.
        let attempts: [(SampleType, usize); 4] = [
            (SampleType::Float, 32),
            (SampleType::Int, 32),
            (SampleType::Int, 24),
            (SampleType::Int, 16),
        ];
        let mut format = None;
        for (st, bits) in attempts.iter() {
            let desired = WaveFormat::new(*bits, *bits, st, sample_rate as usize, channels, None);
            if let Ok(fmt) = audio_client.is_supported_exclusive_with_quirks(&desired) {
                format = Some(fmt);
                break;
            }
        }
        let format = format.ok_or_else(|| {
            anyhow!("no supported exclusive format at {sample_rate} Hz on {device_name:?} (tried Float32/Int32/Int24/Int16)")
        })?;

        let sr = format.get_samplespersec() as f32;
        let blockalign = format.get_blockalign() as usize;
        let ch = format.get_nchannels() as usize;
        let bits = format.get_bitspersample();
        let stype = format
            .get_subformat()
            .map_err(|e| anyhow!("get_subformat: {e:?}"))?;
        let bytes_per_sample = blockalign / ch.max(1);

        let (_def_period, min_period) = audio_client
            .get_device_period()
            .map_err(|e| anyhow!("get_device_period: {e:?}"))?;
        let period = audio_client
            .calculate_aligned_period_near(3 * min_period / 2, Some(128), &format)
            .map_err(|e| anyhow!("calculate_aligned_period_near: {e:?}"))?;
        let mode = StreamMode::EventsExclusive { period_hns: period };
        audio_client
            .initialize_client(&format, &Direction::Render, &mode)
            .map_err(|e| anyhow!("initialize_client (exclusive {sample_rate} Hz): {e:?}"))?;

        let h_event = audio_client
            .set_get_eventhandle()
            .map_err(|e| anyhow!("set_get_eventhandle: {e:?}"))?;
        let render_client = audio_client
            .get_audiorenderclient()
            .map_err(|e| anyhow!("get_audiorenderclient: {e:?}"))?;

        let amp = dbfs_to_amp(dbfs);
        let total: u64 = (dur_secs * sr) as u64;
        let fade: u64 = (((fade_ms / 1000.0) * sr) as u64).clamp(1, total.max(2) / 2);
        let mut n: u64 = 0;

        audio_client
            .start_stream()
            .map_err(|e| anyhow!("start_stream: {e:?}"))?;

        // Watchdog: never render longer than the pulse plus a small margin.
        let watchdog = Instant::now();
        let deadline = Duration::from_secs_f32(dur_secs + 5.0);
        let result = (|| -> Result<()> {
            loop {
                if watchdog.elapsed() > deadline {
                    break;
                }
                let frames = audio_client
                    .get_available_space_in_frames()
                    .map_err(|e| anyhow!("get_available_space_in_frames: {e:?}"))?
                    as usize;
                if frames == 0 {
                    let _ = h_event.wait_for_event(200);
                    continue;
                }
                let mut data = vec![0u8; frames * blockalign];
                for frame in data.chunks_exact_mut(blockalign) {
                    let v = if n >= total {
                        0.0f32
                    } else {
                        let mut s = crate::audio::sine_sum(freqs, n as usize, sr);
                        s *= amp;
                        if n < fade {
                            s *= 0.5 * (1.0 - (PI * n as f32 / fade as f32).cos());
                        } else if n > total - fade {
                            let j = (total - n) as f32;
                            s *= 0.5 * (1.0 - (PI * j / fade as f32).cos());
                        }
                        s
                    };
                    for chan in frame.chunks_exact_mut(bytes_per_sample) {
                        pack_sample(v, &stype, bits, chan);
                    }
                    n += 1;
                }
                render_client
                    .write_to_device(frames, &data, None)
                    .map_err(|e| anyhow!("write_to_device: {e:?}"))?;
                if n >= total {
                    // wrote the last audio; let one buffer drain, then finish
                    let _ = h_event.wait_for_event(500);
                    break;
                }
                if h_event.wait_for_event(1000).is_err() {
                    break;
                }
            }
            Ok(())
        })();

        // Always stop; the client also releases the device on drop.
        let _ = audio_client.stop_stream();
        result
    }
}

// ============================================================================
// Linux implementation (direct ALSA PCM / cpal stream at requested rate)
// ============================================================================
#[cfg(not(windows))]
mod imp {
    use super::*;
    use anyhow::Context;
    use cpal::traits::{DeviceTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample};

    fn resolve_device(name: &str) -> Result<cpal::Device> {
        crate::audio::resolve_output_device(Some(name))
    }

    /// Probe which sample rates the device accepts on Linux (via ALSA/cpal supported output configs).
    pub fn probe_exclusive_support(device_name: &str, rates: &[u32]) -> Result<Vec<String>> {
        let device = resolve_device(device_name)?;
        let configs: Vec<cpal::SupportedStreamConfigRange> = device
            .supported_output_configs()
            .context("querying supported output configs")?
            .collect();
        let mut out = Vec::new();
        for &rate in rates {
            let matching = configs
                .iter()
                .find(|r| r.min_sample_rate() <= rate && rate <= r.max_sample_rate());
            match matching {
                Some(r) => out.push(format!(
                    "{rate:>7} Hz: OK ({:?}, {} ch)   max output {} Hz",
                    r.sample_format(),
                    r.channels(),
                    rate / 2
                )),
                None => out.push(format!("{rate:>7} Hz: not supported")),
            }
        }
        Ok(out)
    }

    /// Play one pulse at an explicit `sample_rate` on `device_name`.
    ///
    /// On Linux, when `device_name` is an ALSA hardware device (e.g. `hw:CARD=...` or `plughw:CARD=...`),
    /// opening the stream with `sample_rate` negotiates the rate directly with the ALSA driver in
    /// exclusive hardware mode (or via PipeWire/ALSA at the requested rate). If the exact requested
    /// `sample_rate` isn't supported by the hardware (e.g. config defaults to 384 kHz on a 96 kHz DAC),
    /// we pick the highest supported rate that clears Nyquist for `freqs`.
    pub fn play_pulse_exclusive(
        device_name: &str,
        freqs: &[f32],
        dbfs: f32,
        dur_secs: f32,
        sample_rate: u32,
        fade_ms: f32,
    ) -> Result<()> {
        let device = resolve_device(device_name)?;
        let top_freq = freqs.iter().copied().fold(0.0f32, f32::max);
        let min_needed = crate::device_guard::required_rate(top_freq);

        let ranges: Vec<cpal::SupportedStreamConfigRange> = device
            .supported_output_configs()
            .context("querying supported output configs")?
            .collect();

        // 1. Prefer the exact requested sample_rate if supported.
        // 2. Otherwise fall back to the highest supported rate on the device that clears Nyquist.
        let chosen = ranges
            .iter()
            .find(|r| r.min_sample_rate() <= sample_rate && sample_rate <= r.max_sample_rate())
            .map(|r| r.with_sample_rate(sample_rate))
            .or_else(|| {
                ranges
                    .iter()
                    .filter(|r| r.max_sample_rate() >= min_needed)
                    .max_by_key(|r| r.max_sample_rate())
                    .map(|r| r.with_max_sample_rate())
            })
            .ok_or_else(|| {
                anyhow!(
                    "no supported output format at {sample_rate} Hz (or >= {min_needed} Hz) on {device_name:?}"
                )
            })?;

        let sample_format = chosen.sample_format();
        let config: cpal::StreamConfig = chosen.into();
        crate::audio::check_nyquist(freqs, config.sample_rate as f32)?;

        let amp = dbfs_to_amp(dbfs);
        let f = freqs.to_vec();
        match sample_format {
            SampleFormat::F32 => run_stream::<f32>(&device, config, f, amp, dur_secs, fade_ms),
            SampleFormat::I32 => run_stream::<i32>(&device, config, f, amp, dur_secs, fade_ms),
            SampleFormat::I16 => run_stream::<i16>(&device, config, f, amp, dur_secs, fade_ms),
            SampleFormat::U16 => run_stream::<u16>(&device, config, f, amp, dur_secs, fade_ms),
            other => bail!("unsupported sample format: {other:?}"),
        }
    }

    fn run_stream<T>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
        freqs: Vec<f32>,
        amp: f32,
        dur_secs: f32,
        fade_ms: f32,
    ) -> Result<()>
    where
        T: SizedSample + FromSample<f32>,
    {
        let sr = config.sample_rate as f32;
        let channels = config.channels as usize;
        let total: usize = (dur_secs * sr) as usize;
        let fade: usize = (((fade_ms / 1000.0) * sr) as usize)
            .min(total.max(2) / 2)
            .max(1);

        let mut n: usize = 0;
        let stream = device.build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    let value = if n >= total {
                        0.0
                    } else {
                        let mut s = crate::audio::sine_sum(&freqs, n, sr);
                        s *= amp;
                        if n < fade {
                            s *= 0.5 * (1.0 - (PI * n as f32 / fade as f32).cos());
                        } else if n > total - fade {
                            let j = (total - n) as f32;
                            s *= 0.5 * (1.0 - (PI * j / fade as f32).cos());
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
        let play_secs = dur_secs + fade_ms / 1000.0;
        std::thread::sleep(Duration::from_secs_f32(play_secs + 0.2));
        Ok(())
    }
}

pub use imp::{play_pulse_exclusive, probe_exclusive_support};

