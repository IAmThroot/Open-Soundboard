use log::warn;

use cpal::traits::HostTrait;

pub const SYSTEM_DEFAULT_LABEL: &str = "System Default";

pub fn device_display_name(device: &cpal::Device) -> Option<String> {
    use cpal::traits::DeviceTrait as _;
    device
        .description()
        .map(|description| description.name().to_owned())
        .ok()
}

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

pub fn find_device_by_name(name: &str) -> Option<cpal::Device> {
    if name == SYSTEM_DEFAULT_LABEL {
        return None;
    }
    cpal::default_host()
        .output_devices()
        .ok()?
        .find(|device| device_display_name(device).as_deref() == Some(name))
}

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

pub fn find_input_device_by_name(name: &str) -> Option<cpal::Device> {
    if name == SYSTEM_DEFAULT_LABEL {
        return None;
    }
    cpal::default_host()
        .input_devices()
        .ok()?
        .find(|device| device_display_name(device).as_deref() == Some(name))
}
