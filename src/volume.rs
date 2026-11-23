//! Pin a render device's master volume to a percent — a safety cap so the pulse level is
//! reproducible regardless of any manual slider. Re-applied periodically while the engine runs, so
//! a mid-session slider move doesn't stick. Failures are logged, never fatal.

#[cfg(windows)]
mod imp {
    use anyhow::{Result, anyhow};
    use wasapi::{DeviceEnumerator, Direction};
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator};
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
    use windows::core::PCWSTR;

    /// Resolve a render endpoint to its raw `IMMDevice` by friendly name.
    ///
    /// Shared with `device_guard`, which needs the same device to reach its property store — hence
    /// this being `pub(crate)` rather than folded into `endpoint_volume`.
    pub(crate) fn immdevice_by_name(device_name: &str) -> Result<IMMDevice> {
        let _ = wasapi::initialize_mta();
        let e = DeviceEnumerator::new().map_err(|x| anyhow!("enumerator: {x:?}"))?;
        let mut dev_id: Option<String> = None;
        for d in &e
            .get_device_collection(&Direction::Render)
            .map_err(|x| anyhow!("collection: {x:?}"))?
        {
            let d = d.map_err(|x| anyhow!("device: {x:?}"))?;
            if d.get_friendlyname().map_err(|x| anyhow!("name: {x:?}"))? == device_name {
                dev_id = Some(d.get_id().map_err(|x| anyhow!("id: {x:?}"))?);
                break;
            }
        }
        let dev_id = dev_id.ok_or_else(|| anyhow!("render device not found: {device_name:?}"))?;
        unsafe {
            let enumr: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|x| anyhow!("CoCreateInstance: {x:?}"))?;
            let wide: Vec<u16> = dev_id.encode_utf16().chain(std::iter::once(0)).collect();
            enumr
                .GetDevice(PCWSTR(wide.as_ptr()))
                .map_err(|x| anyhow!("GetDevice: {x:?}"))
        }
    }

    /// Resolve a render endpoint's `IAudioEndpointVolume` by friendly name.
    fn endpoint_volume(device_name: &str) -> Result<IAudioEndpointVolume> {
        let immdev = immdevice_by_name(device_name)?;
        unsafe {
            immdev
                .Activate(CLSCTX_ALL, None)
                .map_err(|x| anyhow!("Activate IAudioEndpointVolume: {x:?}"))
        }
    }

    /// Read the render endpoint's current master volume as a percent (0..=100).
    pub fn get_endpoint_volume_percent(device_name: &str) -> Result<u8> {
        let vol = endpoint_volume(device_name)?;
        let scalar = unsafe {
            vol.GetMasterVolumeLevelScalar()
                .map_err(|x| anyhow!("GetMasterVolumeLevelScalar: {x:?}"))?
        };
        Ok((scalar * 100.0).round().clamp(0.0, 100.0) as u8)
    }

    /// Set the render endpoint `device_name`'s master scalar volume to `percent`/100 (0.0..=1.0).
    pub fn set_endpoint_volume_percent(device_name: &str, percent: u8) -> Result<()> {
        let scalar = (percent.min(100) as f32) / 100.0;
        let vol = endpoint_volume(device_name)?;
        unsafe {
            vol.SetMasterVolumeLevelScalar(scalar, std::ptr::null())
                .map_err(|x| anyhow!("SetMasterVolumeLevelScalar: {x:?}"))?;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use anyhow::{Context, Result, anyhow, bail};
    use alsa::mixer::{Mixer, SelemChannelId, SelemId};

    /// Extract the ALSA card control identifier (e.g. `"hw:CARD=KA11"` or `"hw:1"`) from a cpal
    /// ALSA device name such as `"hw:CARD=KA11,DEV=0 (KA11, USB Audio)"`, `"hw:CARD=3,DEV=0"`,
    /// `"iec958:CARD=KA11,DEV=0"`, or `"hw:1,0"`.
    pub(crate) fn alsa_card_ctl_name(device_name: &str) -> Option<String> {
        let s = device_name.trim();
        if let Some(pos) = s.find("CARD=") {
            let after = &s[pos + 5..];
            let card = after.split(&[',', ' ', ';', ')'][..]).next()?.trim();
            if !card.is_empty() {
                if card.chars().all(|c| c.is_ascii_digit()) {
                    return Some(format!("hw:{card}"));
                }
                return Some(format!("hw:CARD={card}"));
            }
        }
        if let Some(rest) = s.strip_prefix("hw:").or_else(|| s.strip_prefix("plughw:")) {
            let card = rest.split(&[',', ' '][..]).next()?.trim();
            if !card.is_empty() && card.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Some(format!("hw:{card}"));
            }
        }
        None
    }

    /// Read the ALSA hardware mixer playback volume (or PipeWire/PulseAudio sink volume) as 0..=100%.
    pub fn get_endpoint_volume_percent(device_name: &str) -> Result<u8> {
        if let Some(ctl) = alsa_card_ctl_name(device_name) {
            if let Ok(pct) = get_alsa_mixer_volume(&ctl) {
                return Ok(pct);
            }
        }
        // Fallback: try pactl if available and device_name is a PulseAudio/PipeWire sink or default.
        if let Ok(pct) = get_pactl_volume(device_name) {
            return Ok(pct);
        }
        bail!("no hardware mixer or PulseAudio/PipeWire volume control found for {device_name:?}")
    }

    /// Set the ALSA hardware mixer playback volume (or PipeWire/PulseAudio sink volume) to `percent`%.
    pub fn set_endpoint_volume_percent(device_name: &str, percent: u8) -> Result<()> {
        let pct = percent.min(100);
        let mut any_ok = false;
        let mut last_err = None;

        if let Some(ctl) = alsa_card_ctl_name(device_name) {
            match set_alsa_mixer_volume(&ctl, pct) {
                Ok(()) => any_ok = true,
                Err(e) => last_err = Some(e),
            }
        }
        if set_pactl_volume(device_name, pct).is_ok() {
            any_ok = true;
        }
        if any_ok {
            Ok(())
        } else {
            Err(last_err.unwrap_or_else(|| {
                anyhow!("no volume control available for {device_name:?} (non-fatal)")
            }))
        }
    }

    fn get_alsa_mixer_volume(ctl: &str) -> Result<u8> {
        let mixer = Mixer::new(ctl, false).with_context(|| format!("opening ALSA mixer {ctl}"))?;
        for name in ["PCM", "Master", "Speaker", "Headphone", "Digital"] {
            let id = SelemId::new(name, 0);
            if let Some(selem) = mixer.find_selem(&id) {
                if selem.has_playback_volume() {
                    let (min, max) = selem.get_playback_volume_range();
                    if max > min {
                        let v = selem
                            .get_playback_volume(SelemChannelId::FrontLeft)
                            .with_context(|| format!("reading {name} volume on {ctl}"))?;
                        let pct = (((v - min) as f64 / (max - min) as f64) * 100.0)
                            .round()
                            .clamp(0.0, 100.0) as u8;
                        return Ok(pct);
                    }
                }
            }
        }
        // Fallback: inspect the first element with playback volume
        for elem in mixer.iter() {
            if let Some(selem) = alsa::mixer::Selem::new(elem) {
                if selem.has_playback_volume() {
                    let (min, max) = selem.get_playback_volume_range();
                    if max > min {
                        let v = selem.get_playback_volume(SelemChannelId::FrontLeft)?;
                        let pct = (((v - min) as f64 / (max - min) as f64) * 100.0)
                            .round()
                            .clamp(0.0, 100.0) as u8;
                        return Ok(pct);
                    }
                }
            }
        }
        bail!("no playback volume element on ALSA mixer {ctl}")
    }

    fn set_alsa_mixer_volume(ctl: &str, pct: u8) -> Result<()> {
        let mixer = Mixer::new(ctl, false).with_context(|| format!("opening ALSA mixer {ctl}"))?;
        let mut set_any = false;
        for elem in mixer.iter() {
            if let Some(selem) = alsa::mixer::Selem::new(elem) {
                if selem.has_playback_volume() {
                    let (min, max) = selem.get_playback_volume_range();
                    if max > min {
                        let target = min + (((max - min) as f64 * (pct as f64 / 100.0)).round() as i64);
                        let _ = selem.set_playback_volume_all(target);
                        if selem.has_playback_switch() {
                            let _ = selem.set_playback_switch_all(1);
                        }
                        set_any = true;
                    }
                }
            }
        }
        if set_any {
            Ok(())
        } else {
            bail!("no playback volume element on ALSA mixer {ctl}")
        }
    }

    fn get_pactl_volume(device_name: &str) -> Result<u8> {
        let sink = if device_name.is_empty() || device_name == "default" || device_name == "pulse" || device_name == "pipewire" {
            "@DEFAULT_SINK@"
        } else {
            device_name
        };
        let out = std::process::Command::new("pactl")
            .args(["get-sink-volume", sink])
            .output()
            .context("running pactl")?;
        if !out.status.success() {
            bail!("pactl get-sink-volume failed");
        }
        let s = String::from_utf8_lossy(&out.stdout);
        for tok in s.split_whitespace() {
            if let Some(num) = tok.strip_suffix('%') {
                if let Ok(pct) = num.parse::<u16>() {
                    return Ok(pct.min(100) as u8);
                }
            }
        }
        bail!("could not parse pactl output")
    }

    fn set_pactl_volume(device_name: &str, pct: u8) -> Result<()> {
        // Only invoke pactl when device_name is not a raw ALSA hw/plughw device string,
        // or if it matches a PulseAudio/PipeWire sink name.
        if device_name.contains("CARD=") || device_name.starts_with("hw:") || device_name.starts_with("plughw:") {
            bail!("ALSA direct device; skipping pactl");
        }
        let sink = if device_name.is_empty() || device_name == "default" || device_name == "pulse" || device_name == "pipewire" {
            "@DEFAULT_SINK@"
        } else {
            device_name
        };
        let status = std::process::Command::new("pactl")
            .args(["set-sink-volume", sink, &format!("{pct}%")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("running pactl")?;
        if status.success() {
            Ok(())
        } else {
            bail!("pactl set-sink-volume exited with {status}")
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn extracts_alsa_card_from_cpal_device_strings() {
            assert_eq!(
                alsa_card_ctl_name("hw:CARD=KA11,DEV=0").as_deref(),
                Some("hw:CARD=KA11")
            );
            assert_eq!(
                alsa_card_ctl_name("hw:CARD=3,DEV=0 (Komplete Audio 2, USB Audio)").as_deref(),
                Some("hw:3")
            );
            assert_eq!(
                alsa_card_ctl_name("plughw:CARD=Dongle,DEV=0").as_deref(),
                Some("hw:CARD=Dongle")
            );
            assert_eq!(
                alsa_card_ctl_name("iec958:CARD=PCH,DEV=0").as_deref(),
                Some("hw:CARD=PCH")
            );
            assert_eq!(alsa_card_ctl_name("hw:1,0").as_deref(), Some("hw:1"));
            assert_eq!(alsa_card_ctl_name("default"), None);
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use anyhow::{Result, bail};

    pub fn get_endpoint_volume_percent(_device_name: &str) -> Result<u8> {
        bail!("volume query not implemented on this platform")
    }

    pub fn set_endpoint_volume_percent(_device_name: &str, _percent: u8) -> Result<()> {
        Ok(())
    }
}

pub use imp::{get_endpoint_volume_percent, set_endpoint_volume_percent};
#[cfg(windows)]
pub(crate) use imp::immdevice_by_name;

