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

fn list_devices<I, E>(devices: Result<I, E>, kind: &str) -> Vec<String>
where
    I: IntoIterator<Item = cpal::Device>,
    E: std::fmt::Display,
{
    let mut names = vec![SYSTEM_DEFAULT_LABEL.to_owned()];
    match devices {
        Ok(iter) => {
            for device in iter {
                match device_display_name(&device) {
                    Some(name) => {
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    }
                    None => warn!("Skipping {kind} device with unreadable name"),
                }
            }
        }
        Err(err) => warn!("Could not enumerate {kind} devices: {err}"),
    }
    names
}

fn find_device<I, E>(devices: Result<I, E>, name: &str) -> Option<cpal::Device>
where
    I: Iterator<Item = cpal::Device>,
{
    if name == SYSTEM_DEFAULT_LABEL {
        return None;
    }
    devices
        .ok()?
        .find(|device| device_display_name(device).as_deref() == Some(name))
}

pub fn list_output_devices() -> Vec<String> {
    list_devices(cpal::default_host().output_devices(), "output")
}

pub fn find_device_by_name(name: &str) -> Option<cpal::Device> {
    find_device(cpal::default_host().output_devices(), name)
}

pub fn list_input_devices() -> Vec<String> {
    list_devices(cpal::default_host().input_devices(), "input")
}

pub fn find_input_device_by_name(name: &str) -> Option<cpal::Device> {
    find_device(cpal::default_host().input_devices(), name)
}
