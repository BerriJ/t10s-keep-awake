# Portability / platform boundary

The app targets **Windows** and **Linux**, with the core logic platform-agnostic and OS-specific
parts isolated via `#[cfg(windows)]` / `#[cfg(target_os = "linux")]`. This doc maps that boundary so
a **macOS backend can be contributed** without touching the portable core.

## Module map

| File | Portable? | Windows | Linux |
|---|---|---|---|
| `config.rs` | ✅ portable | `%APPDATA%\T10sKeepAwake\config.toml` | `~/.config/T10sKeepAwake/config.toml` |
| `scheduler.rs` | ✅ portable | engine loop + mode dispatch | engine loop + mode dispatch |
| `monitor.rs` | ✅ cross-platform | WASAPI loopback (`LoopbackMeter`) | `cpal` input/monitor stream (`LoopbackMeter`) |
| `audio.rs` | ✅ portable | shared-mode pulse render via `cpal` | shared-mode pulse render via `cpal` |
| `audio_exclusive.rs` | ✅ cross-platform | WASAPI exclusive for >24 kHz output | Direct ALSA / `cpal` stream negotiated at `sample_rate` |
| `volume.rs` | ✅ cross-platform | `IAudioEndpointVolume` | ALSA hardware mixer (`alsa::mixer`) + `pactl` fallback |
| `device_guard.rs` | ✅ cross-platform | volume pin + `PKEY_AudioEngine_DeviceFormat` repair | volume pin + capability check |
| `health.rs` | ✅ portable | engine status shared with UI | engine status tracking |
| `autostart.rs` | ✅ cross-platform | HKCU `Run` key | `~/.config/systemd/user/t10s-keep-awake.service` |
| `tray.rs` | 🪟 Windows | system-tray + menu via `nwg` | omitted (headless daemon on Linux) |
| `settings_web.rs` | 🪟 Windows | settings window via `wry`/WebView2 | omitted (`settings` subcommand prints config path & TOML) |
| `settings_ui.rs` | 🪟 Windows | fallback settings window in native `nwg` | omitted |

## The ultrasonic pulse is *easier* on macOS
The Windows path needs WASAPI **exclusive mode** to escape the 48 kHz shared mix format and emit
>24 kHz. macOS CoreAudio has no exclusive-mode dance: you set the device's **nominal sample rate**
directly (`kAudioDevicePropertyNominalSampleRate`), then cpal can render at that rate. So a
CoreAudio hi-res output is likely simpler than `audio_exclusive.rs`.

## Suggested shape for a cross-platform version
If someone tackles this, the clean refactor is to put the OS-specific pieces behind small traits and
select the implementation with `#[cfg(target_os = "...")]`:

- `HiResOutput` — render a summed-sine pulse at an arbitrary sample rate (Windows: WASAPI exclusive; macOS: CoreAudio nominal-rate + cpal).
- `LoopbackMeter` — rolling RMS of what's playing on a render device (already an interface in `monitor.rs`).
- `VolumeControl` — get/set an endpoint's volume (may be a no-op).
- `TrayUi` + `Autostart` — the shell.

The portable core (`config`, `scheduler`, `SilenceGate`, the pulse synthesis) stays untouched; only
the backend crates change per platform. Keep the `MAX_LEVEL_DBFS = -12` safety clamp on every backend.
