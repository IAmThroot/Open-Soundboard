use std::collections::HashSet;
use std::path::{Path, PathBuf};

use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::model::{Board, Hotkey, SlotId, SoundSlot};

pub const CONFIG_VERSION: u32 = 1;

pub const STATE_VERSION: u32 = 2;
const CONFIG_FILE_NAME: &str = "config.json";
const BACKUP_SUFFIX: &str = "json.bak";
const V1_BACKUP_SUFFIX: &str = "json.v1.bak";

fn default_volume() -> f32 {
    0.7
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppConfig {
    pub version: u32,
    #[serde(default)]
    pub output_device_name: Option<String>,
    #[serde(default = "default_volume")]
    pub volume: f32,
    #[serde(default)]
    pub stop_hotkey: Option<Hotkey>,
    #[serde(default)]
    pub slots: Vec<SoundSlot>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            output_device_name: None,
            volume: default_volume(),
            stop_hotkey: None,
            slots: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppState {
    pub version: u32,
    #[serde(default = "default_volume")]
    pub volume: f32,
    #[serde(default)]
    pub output_device_name: Option<String>,

    #[serde(default)]
    pub input_device_name: Option<String>,

    #[serde(default)]
    pub close_to_tray: bool,
    #[serde(default)]
    pub stop_hotkey: Option<Hotkey>,
    #[serde(default)]
    pub boards: Vec<Board>,
    #[serde(default)]
    pub active_board: crate::model::BoardId,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            volume: default_volume(),
            output_device_name: None,
            input_device_name: None,
            close_to_tray: false,
            stop_hotkey: None,
            boards: vec![Board::empty(1, "My Sounds")],
            active_board: 1,
        }
    }
}

pub fn config_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "OpenSoundboard", "Open Soundboard")?;
    Some(dirs.config_dir().join(CONFIG_FILE_NAME))
}

pub fn load() -> Option<AppState> {
    load_from(&config_path()?)
}

pub fn save(state: &AppState) -> Result<(), String> {
    save_to(
        &config_path().ok_or_else(|| "No home directory found.".to_owned())?,
        state,
    )
}

fn load_from(path: &Path) -> Option<AppState> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!("Could not read config {}: {err}", path.display());
            return None;
        }
    };

    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(err) => {
            warn!(
                "Config {} is corrupt ({err}); starting fresh.",
                path.display()
            );
            backup(path, BACKUP_SUFFIX);
            return None;
        }
    };

    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(v) if v as u32 == STATE_VERSION => match serde_json::from_value::<AppState>(value) {
            Ok(state) => {
                info!("Loaded state ({} boards).", state.boards.len());
                Some(sanitize_state(state))
            }
            Err(err) => {
                warn!(
                    "Config {} failed to parse ({err}); starting fresh.",
                    path.display()
                );
                backup(path, BACKUP_SUFFIX);
                None
            }
        },
        Some(v) if v as u32 == CONFIG_VERSION => {
            match serde_json::from_slice::<AppConfig>(&bytes) {
                Ok(v1) => {
                    info!("Migrating version-1 config to version 2.");

                    if let Err(err) = std::fs::write(path.with_extension(V1_BACKUP_SUFFIX), &bytes)
                    {
                        warn!("Could not back up v1 config: {err}");
                    }
                    let state = migrate_v1(v1);

                    if let Err(err) = save_to(path, &state) {
                        warn!("Could not write migrated config: {err}");
                    }
                    Some(state)
                }
                Err(err) => {
                    warn!(
                        "Version-1 config {} is corrupt ({err}); starting fresh.",
                        path.display()
                    );
                    backup(path, BACKUP_SUFFIX);
                    None
                }
            }
        }
        other => {
            warn!("Config version {other:?} is not supported; starting fresh.");
            backup(path, BACKUP_SUFFIX);
            None
        }
    }
}

fn save_to(path: &Path, state: &AppState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Could not create {}: {err}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(state)
        .map_err(|err| format!("Could not encode config: {err}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)
        .map_err(|err| format!("Could not write {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|err| format!("Could not publish {}: {err}", path.display()))?;
    Ok(())
}

fn backup(path: &Path, suffix: &str) {
    let backup = path.with_extension(suffix);
    if let Err(err) = std::fs::rename(path, &backup) {
        warn!("Could not back up {}: {err}", path.display());
    } else {
        info!("Backed up {} to {}.", path.display(), backup.display());
    }
}

fn migrate_v1(v1: AppConfig) -> AppState {
    let slots = sanitize_slots(v1.slots);
    let mut stop_hotkey = v1.stop_hotkey;
    if let Some(stop) = stop_hotkey.clone() {
        if slots.iter().any(|s| s.hotkey.as_ref() == Some(&stop)) {
            warn!(
                "Stop key {} clashes with a sound; dropping it.",
                stop.display()
            );
            stop_hotkey = None;
        }
    }
    AppState {
        version: STATE_VERSION,
        volume: v1.volume.clamp(0.0, 1.0),
        output_device_name: v1.output_device_name,
        input_device_name: None,
        close_to_tray: false,
        stop_hotkey,
        boards: vec![Board {
            id: 1,
            name: "My Sounds".to_owned(),
            slots,
        }],
        active_board: 1,
    }
}

fn sanitize_state(mut state: AppState) -> AppState {
    state.volume = state.volume.clamp(0.0, 1.0);
    state.version = STATE_VERSION;
    for board in &mut state.boards {
        board.slots = sanitize_slots(std::mem::take(&mut board.slots));
    }
    if state.boards.is_empty() {
        warn!("State has no boards; creating a fresh one.");
        state.boards.push(Board::empty(1, "My Sounds"));
    }
    if !state.boards.iter().any(|b| b.id == state.active_board) {
        state.active_board = state.boards[0].id;
    }

    if let Some(stop) = state.stop_hotkey.clone() {
        let clash = state
            .boards
            .iter()
            .flat_map(|b| b.slots.iter())
            .any(|s| s.hotkey.as_ref() == Some(&stop));
        if clash {
            warn!(
                "Stop key {} clashes with a sound; dropping it.",
                stop.display()
            );
            state.stop_hotkey = None;
        }
    }
    state
}

fn sanitize_slots(slots: Vec<SoundSlot>) -> Vec<SoundSlot> {
    let mut clean: Vec<SoundSlot> = Vec::with_capacity(slots.len());
    for mut slot in slots {
        let Some(path) = slot.file_path.clone() else {
            continue;
        };
        slot.missing = !path.exists();
        if slot.missing {
            info!(
                "Slot {:?} points at missing file {}; flagged.",
                slot.name,
                path.display()
            );
        }
        clean.push(slot);
    }

    let mut seen: HashSet<Hotkey> = HashSet::new();
    for slot in &mut clean {
        if let Some(hotkey) = slot.hotkey.clone() {
            if !seen.insert(hotkey.clone()) {
                warn!(
                    "Duplicate binding {} on {:?}; unbinding it.",
                    hotkey.display(),
                    slot.name
                );
                slot.hotkey = None;
            }
        }
    }
    clean
}

pub fn next_slot_id(boards: &[Board]) -> SlotId {
    boards
        .iter()
        .flat_map(|b| b.slots.iter().map(|s| s.id))
        .max()
        .unwrap_or(0)
        + 1
}

pub fn next_board_id(boards: &[Board]) -> crate::model::BoardId {
    boards.iter().map(|b| b.id).max().unwrap_or(0) + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn slot(id: SlotId, name: &str, hotkey: Option<Hotkey>, path: Option<&str>) -> SoundSlot {
        SoundSlot {
            id,
            name: name.to_owned(),
            file_path: path.map(PathBuf::from),
            hotkey,
            missing: false,
            trim_start: 0.0,
            trim_end: 1.0,
            duration_secs: None,
        }
    }

    fn state_with(boards: Vec<Board>) -> AppState {
        AppState {
            boards,
            ..AppState::default()
        }
    }

    fn roundtrip_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("open-soundboard-config-tests")
            .join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip_preserves_everything() {
        let dir = roundtrip_dir("roundtrip");
        let path = dir.join(CONFIG_FILE_NAME);
        let state = AppState {
            version: STATE_VERSION,
            output_device_name: Some("CABLE Input".to_owned()),
            input_device_name: Some("Microphone (Realtek)".to_owned()),
            close_to_tray: true,
            volume: 0.42,
            stop_hotkey: Some(Hotkey::simple("F9")),
            active_board: 7,
            boards: vec![
                Board {
                    id: 7,
                    name: "Main".to_owned(),
                    slots: vec![
                        slot(1, "Boom", Some(Hotkey::simple("F6")), Some("/s/boom.mp3")),
                        slot(9, "Gone", None, Some("/nope/missing.mp3")),
                    ],
                },
                Board::empty(10, "Extra"),
            ],
        };
        save_to(&path, &state).unwrap();
        let loaded = load_from(&path).expect("must load");
        assert_eq!(loaded.version, STATE_VERSION);
        assert_eq!(loaded.output_device_name.as_deref(), Some("CABLE Input"));
        assert_eq!(
            loaded.input_device_name.as_deref(),
            Some("Microphone (Realtek)")
        );
        assert!(loaded.close_to_tray);
        assert!((loaded.volume - 0.42).abs() < f32::EPSILON);
        assert_eq!(loaded.stop_hotkey, Some(Hotkey::simple("F9")));
        assert_eq!(loaded.active_board, 7);

        assert_eq!(loaded.boards[0].slots.len(), 2);
        assert!(loaded.boards[0].slots.iter().all(|s| s.missing));
        assert_eq!(loaded.boards[0].slots[0].hotkey, Some(Hotkey::simple("F6")));
        assert!(loaded.boards[1].slots.is_empty());
    }

    #[test]
    fn corrupt_file_is_backed_up_and_ignored() {
        let dir = roundtrip_dir("corrupt");
        let path = dir.join(CONFIG_FILE_NAME);
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(load_from(&path).is_none());
        assert!(path.with_extension(BACKUP_SUFFIX).exists());
        assert!(!path.exists());
    }

    #[test]
    fn wrong_version_is_backed_up_and_ignored() {
        let dir = roundtrip_dir("version");
        let path = dir.join(CONFIG_FILE_NAME);
        std::fs::write(&path, r#"{"version": 99, "boards": []}"#).unwrap();
        assert!(load_from(&path).is_none());
        assert!(path.with_extension(BACKUP_SUFFIX).exists());
    }

    #[test]
    fn version_1_migrates_to_a_single_board_and_preserves_bytes() {
        let dir = roundtrip_dir("migrate");
        let path = dir.join(CONFIG_FILE_NAME);
        let v1 = AppConfig {
            version: CONFIG_VERSION,
            output_device_name: Some("Speakers".to_owned()),
            volume: 0.3,
            stop_hotkey: Some(Hotkey::simple("F9")),
            slots: vec![slot(
                4,
                "Old",
                Some(Hotkey::simple("F6")),
                Some("/o/old.mp3"),
            )],
        };
        let raw = serde_json::to_string_pretty(&v1).unwrap();
        std::fs::write(&path, &raw).unwrap();

        let migrated = load_from(&path).expect("must migrate");
        assert_eq!(migrated.version, STATE_VERSION);
        assert_eq!(migrated.output_device_name.as_deref(), Some("Speakers"));
        assert!((migrated.volume - 0.3).abs() < f32::EPSILON);
        assert_eq!(migrated.stop_hotkey, Some(Hotkey::simple("F9")));
        assert_eq!(migrated.boards.len(), 1);
        assert_eq!(migrated.boards[0].name, "My Sounds");
        assert_eq!(migrated.active_board, migrated.boards[0].id);
        assert_eq!(migrated.boards[0].slots.len(), 1);
        assert_eq!(migrated.boards[0].slots[0].id, 4);

        assert_eq!(
            std::fs::read(path.with_extension(V1_BACKUP_SUFFIX)).unwrap(),
            raw.as_bytes()
        );
        let reread = load_from(&dir.join(CONFIG_FILE_NAME)).expect("must reload v2");
        assert_eq!(reread.version, STATE_VERSION);
        assert_eq!(reread.boards.len(), 1);
    }

    #[test]
    fn missing_file_returns_none_without_backup() {
        let dir = roundtrip_dir("missing");
        let path = dir.join(CONFIG_FILE_NAME);
        let _ = std::fs::remove_file(&path);
        assert!(load_from(&path).is_none());
        assert!(!path.with_extension(BACKUP_SUFFIX).exists());
    }

    #[test]
    fn sanitize_state_enforces_per_board_exclusivity() {
        let key = Hotkey::simple("F6");

        let state = state_with(vec![
            Board {
                id: 1,
                name: "A".to_owned(),
                slots: vec![
                    slot(1, "First", Some(key.clone()), Some("/x/a.mp3")),
                    slot(2, "Second", Some(key.clone()), Some("/x/b.mp3")),
                    slot(3, "Empty", None, None),
                ],
            },
            Board {
                id: 2,
                name: "B".to_owned(),
                slots: vec![slot(4, "Other", Some(key.clone()), Some("/x/c.mp3"))],
            },
        ]);
        let clean = sanitize_state(state);

        assert_eq!(clean.boards[0].slots.len(), 2);
        assert_eq!(clean.boards[0].slots[0].hotkey, Some(key.clone()));
        assert_eq!(clean.boards[0].slots[1].hotkey, None);

        assert_eq!(clean.boards[1].slots[0].hotkey, Some(key));
    }

    #[test]
    fn sanitize_state_drops_stop_key_clashing_on_any_board() {
        let stop = Hotkey::simple("F12");
        let mut state = state_with(vec![
            Board {
                id: 1,
                name: "A".to_owned(),
                slots: vec![slot(1, "First", None, Some("/x/a.mp3"))],
            },
            Board {
                id: 2,
                name: "B".to_owned(),
                slots: vec![slot(2, "Other", Some(stop.clone()), Some("/x/c.mp3"))],
            },
        ]);
        state.stop_hotkey = Some(stop.clone());
        state.active_board = 1;

        let clean = sanitize_state(state);
        assert_eq!(clean.stop_hotkey, None);
    }

    #[test]
    fn sanitize_state_repairs_empty_and_bad_active_board() {
        let mut state = AppState::default();
        state.boards.clear();
        state.active_board = 999;
        let clean = sanitize_state(state);
        assert_eq!(clean.boards.len(), 1);
        assert_eq!(clean.active_board, clean.boards[0].id);

        let mut state2 = state_with(vec![Board::empty(5, "X"), Board::empty(6, "Y")]);
        state2.active_board = 12345;
        let clean2 = sanitize_state(state2);
        assert_eq!(clean2.active_board, 5);
    }

    #[test]
    fn sanitize_clamps_volume_and_keeps_existing_files() {
        let dir = roundtrip_dir("exists");
        let real = dir.join("real.mp3");
        std::fs::write(&real, b"fake audio").unwrap();
        let state = AppState {
            volume: 99.0,
            boards: vec![Board {
                id: 1,
                name: "A".to_owned(),
                slots: vec![slot(1, "Real", None, real.to_str())],
            }],
            ..AppState::default()
        };
        let clean = sanitize_state(state);
        assert_eq!(clean.volume, 1.0);
        assert!(!clean.boards[0].slots[0].missing);
    }

    #[test]
    fn next_ids_count_past_the_maximum() {
        assert_eq!(next_slot_id(&[]), 1);
        assert_eq!(next_board_id(&[]), 1);
        let boards = vec![
            Board {
                id: 2,
                name: "A".to_owned(),
                slots: vec![
                    slot(1, "a", None, Some("/a")),
                    slot(9, "b", None, Some("/b")),
                ],
            },
            Board::empty(5, "B"),
        ];
        assert_eq!(next_slot_id(&boards), 10);
        assert_eq!(next_board_id(&boards), 6);
    }
}
