# 🔊 Open Soundboard

Built by [IAmThroot](https://github.com/IAmThroot).

A dead-simple, blazing-fast, modern desktop soundboard for Windows. Map audio clips to global keyboard hotkeys and play them out through your speakers, headphones, or a virtual audio cable routed directly into Discord, Zoom, OBS, or in-game voice chat.

Designed for gamers, streamers, podcasters, and everyday users who want a soundboard that just works—with zero bloat, instant startup, low latency, and a clean, responsive interface.

---

##  Features

- ** Global System-Wide Hotkeys**  
  Trigger sounds from anywhere—even while tabbed into full-screen games or when the soundboard is minimized to the system tray. Supports single keys (`F1`–`F24`, letters, numbers, numpad, media keys) and composite chords (e.g. `Ctrl+Alt+1`, `Shift+F5`).
- **🗂 Unlimited Soundboards**  
  Organize clips into dedicated boards (e.g. *Gaming*, *Memes*, *D&D*, *Reactions*). Easily add, rename, switch, and delete boards on the fly.
- **✂ Visual Waveform & Trim Editor**  
  Non-destructively trim long clips directly in the app. Inspect the audio waveform, drag start and end handles with millisecond precision, preview the cut, and save.
- **🎙 In-App Microphone Recording**  
  Record voice lines or sound effects directly from your microphone with one click. Recorded takes are automatically saved as 16-bit WAV files and added to your active board as sound tiles. Built with stream-death recovery and watchdog protection to ensure the recorder never wedges or hangs.
- **🎚️ Device Routing & Master Controls**  
  Select your audio output device on the fly—route clips through your headset for personal listening or through a virtual audio cable (like VB-Cable) to broadcast to voice chat. Features a master volume slider and a universal panic/stop hotkey you can bind to any key.
- ** System Tray Integration**  
  Keep your taskbar uncluttered. Enable "Close to Tray" in Settings so the app runs unobtrusively in the background while keeping all global hotkeys fully active.
- ** Automatic Atomic Persistence**  
  No manual save buttons. Your boards, sound configurations, volume levels, and hotkey bindings are saved atomically to `%APPDATA%`, preventing config corruption even during unexpected shutdowns.
- ** Ultra-Lightweight & Native**  
  Written 100% in Rust using `eframe`/`egui`, `rodio`, and `cpal`. Consumes ~0% CPU at idle and starts instantly.

---

##  Installation Guide

Open Soundboard runs on **64-bit Windows 10 and Windows 11**.

### Option 1: Windows Installer (Recommended)

1. Download **`OpenSoundboard-Setup-0.1.1.exe`** from the [Releases](https://github.com/IAmThroot/Open-Soundboard/releases) page.
2. Run the installer.
   - **No Administrator Rights Required:** Open Soundboard installs cleanly per-user into `%LOCALAPPDATA%\Programs\Open Soundboard`.
   - Optionally check the box to create a Desktop shortcut.
3. Launch Open Soundboard from your Start Menu or Desktop.

> **Upgrading / Uninstalling:** When you update or uninstall Open Soundboard, your boards, settings, and custom mic recordings are stored safely in `%APPDATA%\OpenSoundboard` and will remain intact.

### Option 2: Portable Zip

1. Download **`OpenSoundboard-0.1.1-portable.zip`** from the [Releases](https://github.com/IAmThroot/Open-Soundboard/releases) page.
2. Extract the folder anywhere on your computer (e.g., `Documents` or a USB flash drive).
3. Double-click `open-soundboard.exe` to run. No files are written to `Program Files` or the Windows Registry.

> **SmartScreen Note:** Because Open Soundboard is an independent open-source binary, Windows SmartScreen may display an alert on first launch. Click **More info** → **Run anyway**.

---

## 🛠 Building from Source

If you prefer building from source, you will need the Rust toolchain installed (Rust 1.95 or newer).

```bash
# Clone the repository
git clone https://github.com/IAmThroot/Open-Soundboard.git
cd Open-Soundboard

# Run tests and linter
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings

# Build and run the release executable
cargo run --release
```

To compile the Windows installer locally, install [Inno Setup 6](https://jrsoftware.org/isdl.php) and run:
```powershell
cargo build --release
iscc /DMyAppVersion=0.1.1 packaging\installer.iss
```

---

##  Usage Guide

### 1. Adding Sounds
- Click **+ Add Sound** on any empty tile.
- Browse and select your audio file (`.mp3`, `.wav`, `.ogg`, or `.flac`).
- Enter a display label for the sound tile.

### 2. Binding Global Hotkeys
- Click the **[Set Key]** button on any sound tile.
- Press the key or combination you want to bind (for example, `F7`, `Ctrl+Shift+D`, or `Numpad 1`).
- To cancel or clear a key binding, press `Esc` while recording.
- The hotkey will immediately work system-wide, even when Open Soundboard is minimized or playing a game.

### 3. Routing Audio into Discord / Games / Zoom
To play sounds into voice chat so your friends or teammates can hear them:
1. Install a virtual audio device such as **[VB-Audio Virtual Cable](https://vb-audio.com/Cable/)** (free).
2. In Open Soundboard, open the audio device dropdown in the toolbar and select **`CABLE Input (VB-Audio Virtual Cable)`**.
3. In Discord, Zoom, or your game's audio settings, set your **Input Device (Microphone)** to **`CABLE Output (VB-Audio Virtual Cable)`**.
4. *(Optional — Hear your own sounds):* If you want to hear the sounds in your own headphones simultaneously, enable "Listen to this device" for CABLE Output in the Windows Sound Control Panel, or use software like Voicemeeter / SteelSeries Sonar.

### 4. Trimming Audio Clips
- Click the **✎** (edit) icon on any sound tile.
- An interactive waveform modal will appear.
- Drag the **blue start handle** and **blue end handle** to isolate the exact section of the sound you want.
- Click **▶ Play Preview** to test your trim boundaries.
- Click **Save**. Hotkeys and clicks will now play only the trimmed segment.

### 5. Recording from your Microphone
- In the top toolbar, select your physical microphone from the input picker.
- Click **Record**. Speak or play the sound you wish to capture.
- Click **Stop Recording**.
- The recording is instantly encoded and saved to `%APPDATA%\OpenSoundboard\data\recordings\`, and a new tile is added to your board ready for hotkey assignment.

### 6. System Tray & Background Operation
- Click the **⚙ (Settings)** button in the toolbar.
- Toggle **"Minimize / Close to System Tray"**.
- Closing the window now hides Open Soundboard to the notification area near your clock.
- Left-click or double-click the tray icon to restore the window, or right-click to quit.

---

## ⚠ Limitations & Notes

- **Supported Formats:** Open Soundboard decodes **MP3, WAV, OGG (Vorbis), and FLAC**. Formats requiring external proprietary codecs (such as Opus or AAC/M4A) are not supported out of the box; convert them to MP3 or WAV before adding.
- **Administrator Privileges & Games:** Windows security isolates elevated (Administrator) windows from non-elevated windows. If you are playing a game running in Administrator mode (common for games with kernel-level anti-cheat), launch Open Soundboard as Administrator so Windows allows hotkeys to register across the game.
- **Platform Scope:** The core audio engine and UI are cross-platform Rust, but system-wide global hotkeys (`RegisterHotKey`) and the system tray notification icon are tailored specifically for **Windows**.

---

##  License

This project is licensed under the [MIT License](LICENSE).
