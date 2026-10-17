//! Real output-device discovery via `cpal`.
//!
//! The same code runs on both machines involved in this project:
//! - **Dev machine (Linux + KDE Wayland):** typically PipeWire/PulseAudio names.
//! - **Target (Windows / WASAPI):** friendly endpoint names such as
//!   `Speakers (Realtek Audio)` or `CABLE Input (VB-Audio Virtual Cable)`.
//!
//! Nothing here is platform-specific — `cpal` abstracts the host API — but the
//! UX assumes Windows endpoint naming (Phase 3 hotkeys and the installer are
//! Windows-only too).

use log::warn;

use cpal::traits::HostTrait;

/// Pseudo-device shown first in the dropdown. It resolves to the OS default
/// output *at open time*, so it keeps following the user's Windows default
/// even if they plug/unplug headphones later.
pub const SYSTEM_DEFAULT_LABEL: &str = "System Default";

/// Human-readable name of one device, or `None` if the OS would not describe
/// it (broken driver entry — skipped, never fatal).
pub fn device_display_name(device: &cpal::Device) -> Option<String> {
    use cpal::traits::DeviceTrait as _;
    device
        .description()
        .map(|description| description.name().to_owned())
        .ok()
}

/// All output devices, with [`SYSTEM_DEFAULT_LABEL`] first.
///
/// Never fails hard: if enumeration itself errors (rare — broken audio stack),
/// we log and return just the pseudo-device so the app still starts and the
/// audio engine can attempt its own fallback.
pub fn list_output_devices() -> Vec<String> {
    let mut names = vec![SYSTEM_DEFAULT_LABEL.to_owned()];

    let host = cpal::default_host();
    match host.output_devices() {
        Ok(devices) => {
            for device in devices {
                match device_display_name(&device) {
                    Some(name) => {
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    }
                    None => warn!("Skipping output device with unreadable name"),
                }
            }
        }
        Err(err) => warn!("Could not enumerate output devices: {err}"),
    }

    names
}

/// Find an output device by its exact display name.
///
/// Returns `None` for [`SYSTEM_DEFAULT_LABEL`] (callers resolve that via
/// `default_output_device()`) and for names that are no longer present
/// (e.g. unplugged USB / disabled VB-Cable) — the caller falls back.
pub fn find_device_by_name(name: &str) -> Option<cpal::Device> {
    if name == SYSTEM_DEFAULT_LABEL {
        return None;
    }
    cpal::default_host()
        .output_devices()
        .ok()?
        .find(|device| device_display_name(device).as_deref() == Some(name))
}

/// All input (capture) devices, with [`SYSTEM_DEFAULT_LABEL`] first.
///
/// The OS "default microphone" is often a virtual endpoint (VB-Cable's
/// `CABLE Output`, stereo-mix loops, headsets that are not the mic you mean),
/// so the user picks explicitly — silence on the default is a real trap.
pub fn list_input_devices() -> Vec<String> {
    let mut names = vec![SYSTEM_DEFAULT_LABEL.to_owned()];

    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devices) => {
            for device in devices {
                match device_display_name(&device) {
                    Some(name) => {
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    }
                    None => warn!("Skipping input device with unreadable name"),
                }
            }
        }
        Err(err) => warn!("Could not enumerate input devices: {err}"),
    }

    names
}

/// Find an input device by its exact display name.
///
/// `None` for [`SYSTEM_DEFAULT_LABEL`] (the recorder resolves that via
/// `default_input_device()`) or for vanished hardware — the caller falls back.
pub fn find_input_device_by_name(name: &str) -> Option<cpal::Device> {
    if name == SYSTEM_DEFAULT_LABEL {
        return None;
    }
    cpal::default_host()
        .input_devices()
        .ok()?
        .find(|device| device_display_name(device).as_deref() == Some(name))
}
