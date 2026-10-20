use std::path::PathBuf;

pub fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("com", "OpenSoundboard", "Open Soundboard")
}

pub fn config_path() -> Option<PathBuf> {
    project_dirs().map(|d| d.config_dir().join(crate::config::CONFIG_FILE_NAME))
}

pub fn recordings_dir() -> Result<PathBuf, String> {
    project_dirs()
        .map(|dirs| dirs.data_dir().join(crate::record::RECORDINGS_DIR))
        .ok_or_else(|| "Could not locate a user data folder for recordings.".to_owned())
}
