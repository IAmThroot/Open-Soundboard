# Changelog

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

[v0.1.1]: https://github.com/IAmThroot/Open-Soundboard/releases/tag/v0.1.1
