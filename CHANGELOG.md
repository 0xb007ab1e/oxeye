# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Pre-1.0, the public API may change
between minor versions.

## [Unreleased]

### Changed

- **Renamed the project `oxeye` → `intone`** (the prior name was an explicit placeholder). This
  renames the crates (`intone-core/-cli/-linux/-windows/-macos`), the `intone` command, and the
  `INTONE_SPEECH` env var. **Config moved** from `~/.config/oxeye/` to `~/.config/intone/` — copy
  your existing settings over if you have any (`mv ~/.config/oxeye ~/.config/intone`), otherwise
  it starts from defaults. *to intone* = to speak in a clear, measured voice.

### Added

- **Context voices (content vs reader)** — `intone config voice-context content|ui <voice>`
  (`<context> default` removes one) reads application content and the reader's own
  meta-announcements (time, structure summary, by-type navigation, voice-cycle) in different
  voices. Precedence: content = per-language → content voice → default; ui = ui voice → default.
  `config show` lists them. (Voices Phase 4; see [`docs/voices.md`](docs/voices.md).)
- **Automatic per-language voices** — map languages to voices with
  `intone config voice-lang <tag> <voice>` (`<tag> default` removes one); on Linux the reader
  reads each focused object's locale (AT-SPI `locale()`) and switches voice to match before
  speaking. Tags match case-insensitively and by prefix, most-specific first (`en` covers
  `en-US`; an `en-GB` entry wins for British English). No-op where an app only reports the system
  locale — it never picks a wrong voice. (Voices Phase 3; see [`docs/voices.md`](docs/voices.md).)
- **Live voice cycling** — configure a rotation with `intone config rotation <names…>` (empty
  clears it), then press **Ctrl+Alt+V** while the reader is running to switch to the next voice;
  the new voice announces its own name. Adds `[speech] rotation` to settings (core) and reports it
  in `intone config show`. (Voices Phase 2; see [`docs/voices.md`](docs/voices.md).)
- **`intone-cli`** — `intone voices list` enumerates speech-dispatcher output modules and the
  active module's synthesis voices via SSIP. With no filter it prints a per-language summary
  (engines like espeak-ng expose tens of thousands of voices); `--language <tag>` narrows by a
  case-insensitive prefix (e.g. `en`). Documented in [`docs/voices.md`](docs/voices.md) alongside
  espeak-ng (default) and Piper (neural OSS) guidance.
- **`intone-cli`** — speech configuration commands: `intone config voice|module|language|rate|pitch|volume`
  set the synthesis voice, speech-dispatcher output module, language (BCP-47), and rate/pitch/volume
  (0–100, validated). For `voice`/`module`/`language`, the value `default` reverts to the engine
  default; `intone config show` now reports all speech settings. First step of multi-voice support
  (engine-agnostic, OSS-first); voice discovery (`intone voices list`) and espeak-ng/Piper docs follow.

### Fixed

- **`intone-linux`** — the first character typed into a freshly focused field is now announced.
  Typed text is echoed from the AT-SPI insertion event (reading the inserted run straight from
  the field) rather than inferred from a caret delta against a baseline that didn't exist yet
  for the first keystroke. Caret-move dispatch was factored into a pure, unit-tested
  `caret_action`. (Note: apps that don't expose AT-SPI text — e.g. Electron, terminals — still
  can't be echoed; that's an app limitation, not intone.)
- **`intone-linux`** — hotkey setup no longer registers Control/Alt as standalone grabbed
  modifiers in KWin's `KeyboardMonitor.SetKeyGrabs`. KWin *consumes* any keysym in that list,
  so it swallowed every Control/Alt press before the focused app saw it — locking out all
  Ctrl/Alt shortcuts (Ctrl+C, Alt+Tab, VT switching). Only the dedicated `Ctrl+Alt+<letter>`
  combos are grabbed now; bare Control still drives "silence" via pass-through.
- **`intone-linux`** — speech now actually plays. SSIP is strict request/response, but every
  `ssip-client-async` write (`set_client_name`, `set_rate/pitch/volume`, `cancel`, `speak`)
  must have its reply read or the response stream desyncs; intone never read them, so the first
  `speak` consumed a stale reply (`not a message id`) and no audio was produced. Each write is
  now paired with its read, and `say()` uses the correct `speak → check_receiving_data →
  send_lines → receive_message_id` exchange (the old `send_line` never sent the terminating
  `.`). Surfaced only on real audio — `INTONE_SPEECH=text` bypasses SSIP.

## [0.1.0] — 2026-06-22

First public release: a free, open-source, **privacy-respecting**, **cross-platform** screen
reader built core-first in Rust. No telemetry; networking is off by default.

### Added

- **`intone-core`** — platform-agnostic policy, `unsafe`-free and I/O-free:
  - user-defined **exclusions** (suppress / summarize / lower-priority by app · role · name regex);
  - **announcement composition** scaled by **verbosity** (low / medium / high);
  - **structured-navigation** classification + document-order next/previous search;
  - **Grade-1 braille** translation (text → Unicode braille patterns);
  - `Untrusted<T>` trust boundary and log **redaction**; hardened (`0600`) settings storage.
- **`intone-linux`** — AT-SPI2 back-end (KDE Plasma / Wayland verified):
  - focus reading; element **states**, numeric **value**, and single-line text **content**;
  - **caret tracking**, **edit** (insert/delete) and **selection** announcements — password-gated;
  - **structured navigation**: `Ctrl+Alt+S` structure summary; `Ctrl+Alt+{H,B,L,F}` by type
    (`Shift` = previous);
  - **speech** via speech-dispatcher (SSIP) and **braille** rendering; global keys via KWin's
    accessibility `KeyboardMonitor`; `INTONE_SPEECH=text` for headless/remote use.
- **`intone-windows`** — UI Automation back-end (compiled in CI against the real Windows SDK):
  - event-driven focus; **states** (checked/expanded/selected/disabled/required) and **value**;
  - **SAPI** speech; **by-type navigation** `Ctrl+Alt+{H,B,L,F}` via `RegisterHotKey`.
- **`intone-cli`** (`intone`) — manage configuration: exclusion rules and `config verbosity|braille`.
- Dual-licensed **MIT OR Apache-2.0**. Merge-blocking CI: format, clippy, tests, `cargo-audit`,
  `cargo-deny` (license + advisories), SBOM, and a Windows compile job.

### Known limitations

- The Windows back-end is **compile-verified** in CI but not yet runtime-tested on a real
  desktop. Braille **device** output (BrlAPI) is designed but not wired (see
  `docs/braille-transport.md`); macOS (AXAPI) is planned. Heading navigation on Linux/Windows and
  `has_popup` on Windows have documented edge cases. See open issues.

[0.1.0]: https://github.com/0xb007ab1e/intone/releases/tag/v0.1.0
