use std::fs::{File, OpenOptions, TryLockError};

/// Keeps an exclusive advisory lock on a file in the app's data directory for
/// the lifetime of the process. The OS releases it automatically on exit, so a
/// crash cannot leave the app permanently "already running".
pub struct InstanceLock {
    _file: Option<File>,
}

impl InstanceLock {
    fn none() -> Self {
        Self { _file: None }
    }
}

pub fn acquire() -> Result<InstanceLock, String> {
    let Some(dirs) = crate::paths::project_dirs() else {
        log::warn!("No user data folder; starting without a single-instance lock.");
        return Ok(InstanceLock::none());
    };
    let dir = dirs.data_dir();
    if let Err(err) = std::fs::create_dir_all(dir) {
        log::warn!(
            "Could not create {} ({err}); starting without a single-instance lock.",
            dir.display()
        );
        return Ok(InstanceLock::none());
    }

    let path = dir.join("instance.lock");
    let file = match OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(err) => {
            log::warn!(
                "Could not open {} ({err}); starting without a single-instance lock.",
                path.display()
            );
            return Ok(InstanceLock::none());
        }
    };

    match file.try_lock() {
        Ok(()) => Ok(InstanceLock { _file: Some(file) }),
        Err(TryLockError::WouldBlock) => Err(
            "Open Soundboard is already running.\n\nSwitch to the existing window, or quit it \
             from the system tray before starting a second copy."
                .to_owned(),
        ),
        // Some filesystems do not support advisory locks. Don't block startup
        // for that; the worst case is the old behaviour (two copies).
        Err(TryLockError::Error(err)) => {
            log::warn!("Single-instance lock unavailable ({err}); continuing without it.");
            Ok(InstanceLock { _file: Some(file) })
        }
    }
}
