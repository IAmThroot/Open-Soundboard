# Changelog

## [v0.1.4] — 2026-11-17

### User experience
- **UI:** reorganized navigation around a responsive board sidebar, with a compact selector in narrow windows. Search filters the active board; master output and volume controls stay at the bottom.
- **Routing:** the footer continuously shows the actual output and any fallback or unavailable status while preserving the selected device preference.
- **Sound management:** add multiple supported audio files at once or drop files onto the app; rename and reorder sounds, adjust per-sound volume, and undo recent edits.
- **Boards:** import and export self-contained `.osboard` bundles with their audio. Imported boards receive new IDs and unassigned hotkeys.
- **Recording:** moved microphone selection and controls into a dedicated panel with a live input meter, timer, start, save, and cancel actions.
- **Playback:** per-sound gain applies to both clicks and global hotkeys. UI state tracks pending playback, and recording/output use explicit states.

### Limits
- Undo keeps up to 20 snapshots for the current session. Bundles allow up to 64 MB of audio and up to 256 MB encoded size.


## [v0.1.3] — 2026-10-31

### Bug fixes
- **Audio recovery:** stream errors now invalidate the failed output and finish its players. Play, Test Sound, or device refresh retries the requested output; stale errors from replaced streams are ignored.
- **Routing:** fallback output is reported without overwriting the requested device preference.
- **Hotkeys:** output-device failures no longer mark valid audio files broken or disable their bindings. Hotkeys remain suspended throughout key capture, including asynchronous rebuilds.
- **Playback state:** a failed retrigger stops the previous player so tile indicators and Stop controls remain accurate.
- **Recording:** microphone attempts carry IDs and cancellation tokens. A startup timeout retains one worker, disables retries until it returns, and discards cancelled takes. Restart the app if the driver never returns. Stale events and recorder disconnections are handled explicitly.
- **Waveforms:** enforce the 16-result cache limit on completion and cap outstanding work at 16. Invalidated requests cannot overwrite newer results; an editor waiting for capacity retries after completion.
- **Release:** validate tag/package version agreement and run tests, formatting, and lint before packaging.

### Validation
- Added hardware-independent regression tests for device failures, playback state, microphone cancellation, waveform limits, and hotkey capture.

## [v0.1.2] — 2026-10-20

### Bug fixes
- **Hotkeys:** after the audio engine restarted, presses still reached the dead engine and were silently lost. The listener now receives the new audio sender whenever the engine restarts, and a press that still lands on a dead engine is reported back to the UI, which restarts the engine and retries it (instead of dropping the action).
- **Hotkeys:** rejected-grab warnings and ⚠ badges now use the same label as the binding (`Q`, `7`, `Down`) instead of the OS code name (`KeyQ`, `Digit7`, `ArrowDown`), so the badge appears for letters, digits, and arrows. Partially registered twin keys (top-row + numpad) no longer show a false rejection when the other twin works.
- **Trim preview:** preview playback is now tracked, so its Play button toggles to Stop, the editor stops it on close/save/reset, and Stop All is enabled while it plays.
- **Recording:** the 10-minute cap, a microphone failure, and dropped audio chunks are now reported as distinct outcomes instead of "the microphone stopped early". Disk write errors abort the take and delete the partial file instead of silently truncating it.
- **Recording:** a take is added to the board that was active when recording started, even if you switch boards while recording.
- **Boards:** switching or deleting a board cancels an in-progress hotkey capture instead of writing the key to a hidden board. Deleting a board that isn't open no longer jumps you to a different board, and any of its sounds that are still playing are stopped.
- **Persistence:** config writes (including `fsync`) no longer block the UI thread; a background saver serialises snapshots and the exit path flushes the newest state.
- **Startup:** a second instance now exits with a message instead of both copies racing on `config.json`.
- **Waveforms:** proportional bucket mapping removes the flat, repeated tail that short clips displayed.
- **Build:** Windows version metadata is generated from `Cargo.toml`, removing the duplicated `0.1.1` constants.
- **Formatting:** ran `cargo fmt` so the CI check passes.

### Polish
- Re-linking a sound stops its old playback, resets the trim selection, and refreshes an open trim editor's waveform.
- README no longer claims literal `0% CPU` and documents the 10-minute take cap and single-instance behaviour.

## [v0.1.1] — 2026-10-18

### Bug fixes
- **Audio:** re-picking a device that previously failed no longer leaves the engine silent. The open check now considers whether the sink is actually open, so re-selecting a working device retries instead of skipping.
- **Trim editor:** the editor no longer renders one frame behind the drag, and `Save` stores the current bounds instead of stale ones.
- **Waveform cache:** deleting a slot or board now invalidates its cached waveform, and slot IDs are monotonic within a session so a deleted slot's ID can never resurrect a stale waveform for a new slot.
- **Tray:** closing to the tray while recording a hotkey now cancels the recording and restores global hotkey grabs instead of leaving them suspended forever.
- **Persistence:** a failed config save is no longer silently dropped — the change stays dirty and is retried on the next mutation and on exit.
- **Stop key:** load-time sanitization now drops a Stop key that clashes with a sound slot on *any* board, not just the active one.
- **Tests:** removed a redundant `usable` binding in the audio device test.

### Polish
- **Hotkeys:** the rejected-grab toast now names the actual key (letters, F-keys, numpad digits) instead of a generic `"key"`. Standalone media keys, navigation keys, and toggle keys no longer trigger a false "fires while you type" warning.
- **Mic recording:** the microphone picker is frozen while a recording start is in flight, so the chosen device matches the take already sent.
- **Build:** `build.rs` no longer emits a `cargo:warning` for a successfully embedded icon.
- **CI:** added a `cargo fmt --check` step.
- **MSRV:** corrected the declared Rust version from 1.85 to 1.95 (matching egui/eframe 0.37 requirements).
- **README:** fixed inaccurate claims about per-board volume sliders and an `Esc`-default stop key; updated download links for v0.1.1.
- **Tests:** added unit tests for `app.rs` helpers (`fmt_time`, `ellipsize`, `shorten_device`, `trim_spec_for`), model `looks_like_typing`, and config stop-key clash handling on any board.

[v0.1.2]: https://github.com/IAmThroot/Open-Soundboard/releases/tag/v0.1.2
[v0.1.1]: https://github.com/IAmThroot/Open-Soundboard/releases/tag/v0.1.1

[v0.1.4]: https://github.com/IAmThroot/Open-Soundboard/releases/tag/v0.1.4
[v0.1.3]: https://github.com/IAmThroot/Open-Soundboard/releases/tag/v0.1.3
