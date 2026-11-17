//! Self-contained board bundles. Original paths are never trusted on import.
use crate::model::Board;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
const MAX_AUDIO: u64 = 64 * 1024 * 1024;
const MAX_BUNDLE: u64 = 256 * 1024 * 1024;
#[derive(Serialize, Deserialize)]
struct Bundle {
    version: u32,
    board: Board,
    audio: Vec<Vec<u8>>,
}

pub fn export(board: Board, destination: &Path) -> Result<(), String> {
    let mut audio = Vec::new();
    let mut total = 0;
    for slot in &board.slots {
        let path = slot.file_path.as_ref().ok_or("A sound has no audio file")?;
        let bytes = read_limited(path, MAX_AUDIO.saturating_sub(total))?;
        total += bytes.len() as u64;
        audio.push(bytes);
    }
    let bytes = serde_json::to_vec(&Bundle {
        version: 1,
        board,
        audio,
    })
    .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BUNDLE {
        return Err("Board bundle exceeds 256 MB".into());
    }
    // Reserve a unique temporary sibling; publish only a complete bundle.
    let temp = destination.with_extension(format!("osboard.tmp.{}", std::process::id()));
    let mut owned_temp = false;
    let result = (|| {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| e.to_string())?;
        owned_temp = true;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        drop(file);
        fs::rename(&temp, destination).map_err(|e| e.to_string())
    })();
    if result.is_err() && owned_temp {
        let _ = fs::remove_file(temp);
    }
    result
}
fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(limit + 1)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() as u64 > limit {
        return Err("Board audio exceeds the 64 MB limit".into());
    }
    Ok(data)
}
pub fn import(source: &Path, root: &Path) -> Result<Board, String> {
    let bytes = read_limited(source, MAX_BUNDLE)?;
    let mut bundle: Bundle = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if bundle.version != 1 || bundle.board.slots.len() != bundle.audio.len() {
        return Err("Invalid or unsupported board bundle".into());
    }
    if bundle.audio.iter().map(|a| a.len() as u64).sum::<u64>() > MAX_AUDIO {
        return Err("Board audio exceeds 64 MB".into());
    }
    fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let dir: PathBuf = (0..)
        .map(|n| root.join(format!("import-{}-{n}", std::process::id())))
        .find_map(|p| match fs::create_dir(&p) {
            Ok(()) => Some(Ok(p)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
            Err(e) => Some(Err(e.to_string())),
        })
        .unwrap()?;
    let result = (|| {
        for (index, (slot, bytes)) in bundle.board.slots.iter_mut().zip(bundle.audio).enumerate() {
            // Decoder detects format from content, never use bundle paths as destinations.
            let path = dir.join(format!("sound-{index}.audio"));
            fs::write(&path, bytes).map_err(|e| e.to_string())?;
            slot.file_path = Some(path);
            slot.missing = false;
            slot.hotkey = None;
            slot.volume = if slot.volume.is_finite() {
                slot.volume.clamp(0.0, 1.0)
            } else {
                1.0
            };
            if !slot.trim_start.is_finite()
                || !slot.trim_end.is_finite()
                || slot.trim_start < 0.0
                || slot.trim_end > 1.0
                || slot.trim_start >= slot.trim_end
            {
                slot.clear_trim();
            }
        }
        Ok(bundle.board)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(dir);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Hotkey, SoundSlot};
    #[test]
    fn bundle_roundtrip_embeds_audio_and_ignores_original_paths() {
        let root =
            std::env::temp_dir().join(format!("soundboard-bundle-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.wav");
        fs::write(&source, b"test audio bytes").unwrap();
        let mut board = Board::empty(1, "Test");
        let mut slot = SoundSlot::with_file(1, "Custom name", &source);
        slot.volume = 0.4;
        slot.hotkey = Some(Hotkey::simple("F9"));
        board.slots.push(slot);
        let bundle = root.join("test.osboard");
        export(board, &bundle).unwrap();
        fs::remove_file(source).unwrap();
        let restored = import(&bundle, &root.join("restored")).unwrap();
        assert_eq!(restored.slots[0].volume, 0.4);
        assert!(restored.slots[0].hotkey.is_none());
        assert_eq!(
            fs::read(restored.slots[0].file_path.as_ref().unwrap()).unwrap(),
            b"test audio bytes"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rejects_unknown_bundle_versions_before_writing_audio() {
        let root =
            std::env::temp_dir().join(format!("soundboard-invalid-bundle-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("bad.osboard");
        fs::write(
            &file,
            serde_json::to_vec(&Bundle {
                version: 99,
                board: Board::empty(1, "bad"),
                audio: vec![],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(import(&file, &root.join("audio")).is_err());
        assert!(!root.join("audio").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
