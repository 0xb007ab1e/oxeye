# Voices & speech configuration

intone speaks through **speech-dispatcher** on Linux, which is *engine-agnostic*: it fronts
multiple text-to-speech **output modules** (espeak-ng, Piper, mbrola, …), each exposing one or
more **voices**. intone therefore supports any voice your speech-dispatcher install offers — pick
one with the `intone config` commands below. The defaults are fully open-source (espeak-ng), and
intone never requires a proprietary/cloud engine.

## Configuring speech

All settings persist to the user config file (`intone exclusions path` prints its location) and
take effect the next time the reader starts.

| Command | Effect |
|---|---|
| `intone config voice <name>` | Set the synthesis voice (engine-specific name). `default` clears it. |
| `intone config module <name>` | Set the speech-dispatcher output module (e.g. `espeak-ng`, `piper`). `default` clears it. |
| `intone config language <tag>` | Set the language as a BCP-47 tag (e.g. `en`, `es`). `default` clears it. |
| `intone config rate <0–100>` | Speaking rate (50 = normal). |
| `intone config pitch <0–100>` | Voice pitch (50 = normal). |
| `intone config volume <0–100>` | Volume (100 = full). |
| `intone config show` | Print the current configuration, including all speech settings. |

For `voice` / `module` / `language`, the literal value `default` reverts to the engine default.
Levels are validated 0–100 (a value above 100 is rejected, not silently clamped).

## Discovering voices

```console
$ intone voices list
output modules: espeak-ng
14805 voices across 140 languages — refine with `intone voices list --language <tag>`:
  en-GB (105)
  en-US (105)
  …

$ intone voices list --language en
voices for language 'en' (945):
  English (Great Britain) [en-GB]
  English (Great Britain)+Alan [en-GB-Alan]
  …
```

With no filter, `voices list` prints a **per-language summary** (engines like espeak-ng expose
tens of thousands of voices — every language × speaker variant). `--language <tag>` is a
case-insensitive **prefix** match, so `en` lists every English locale; a long result is capped
and the remainder summarised.

Voices are reported for the **currently selected output module**. To browse another module's
voices, switch to it first (`intone config module <name>`) and re-run `intone voices list`.

> `intone voices list` queries the speech-dispatcher daemon; if it isn't running, start it once
> (e.g. `spd-say hello`) and retry.

## Switching voices on the fly

Configure a list of voices to cycle through, then press **Ctrl+Alt+V** while the reader is
running to jump to the next one — the new voice announces its own name so you hear it
immediately:

```console
$ intone config rotation Alan Klaus "English (Great Britain)"   # set the cycle (in order)
$ intone config rotation                                        # no names clears it
$ intone config show                                            # shows "voice rotation: …"
```

Each press advances through the list and wraps around. With no rotation configured, Ctrl+Alt+V
says so. (The switch applies to the running session; the persisted default voice is still
`intone config voice <name>`.)

## Per-language voices (automatic)

Map languages to voices and intone switches automatically when the focused content's language
changes — e.g. an English voice for English text, a Spanish voice for Spanish:

```console
$ intone config voice-lang en "English (Great Britain)"
$ intone config voice-lang es "Spanish (Spain)"
$ intone config voice-lang en default        # remove the mapping for en
$ intone config show                          # shows "language voices: en→…, es→…"
```

Tags match case-insensitively and by prefix, **most-specific first** — so `en` covers `en-US`
and `en-GB`, while a separate `en-GB` entry takes precedence for British English.

How the language is detected: intone reads the focused object's locale from AT-SPI
(`AccessibleProxy::locale()`) on each focus change and applies the matching voice before speaking.
**Caveat:** many toolkits report the *system* locale for every object rather than a true
per-object language, so auto-switching only takes effect where an application genuinely exposes a
differing locale (some document/web content). Otherwise it's a no-op — it never picks a wrong
voice, it just doesn't switch.

## Content vs reader voices

Give the application's content and the reader's own announcements different voices, so the
reader's meta-chatter is audibly distinct from what it's reading:

```console
$ intone config voice-context content "English (Great Britain)"
$ intone config voice-context ui "English (Scotland)"
$ intone config voice-context ui default      # remove the ui voice
$ intone config show                           # shows "context voices: …"
```

- **content** — application content: focus readout, caret/typed text, selections.
- **ui** — the reader's own meta-announcements: the time (Ctrl+Alt+O), the structure summary
  (Ctrl+Alt+S), by-type navigation (Ctrl+Alt+H/B/L/F), and voice-cycle confirmations.

Precedence — **content:** a per-language voice (above) wins, then the content context voice, then
the default voice (`intone config voice <name>`); **ui:** the ui context voice, then the default.
Set a default voice as the baseline so any unmatched case still has a voice to fall back to.

## Output modules (OSS engines)

### espeak-ng — the default

[espeak-ng](https://github.com/espeak-ng/espeak-ng) ships with speech-dispatcher and needs no
setup. It is compact, extremely responsive, and covers 100+ languages. It is the default module,
so intone works out of the box with no configuration. Its voices are formant-synthesis (robotic),
which many screen-reader users prefer for speed and intelligibility at high rates.

### Piper — neural, higher quality (optional)

[Piper](https://github.com/OHF-Voice/piper1-gpl) produces natural neural voices, fully offline
and open-source. speech-dispatcher integrates Piper through a **generic** output module (a small
shell command), not a built-in module, so it takes some one-time setup:

1. Install Piper and download voice models (`.onnx` + `.onnx.json`) for your language from the
   Piper voices page.
2. Add a generic module + `AudioOutputMethod` to `~/.config/speech-dispatcher/speechd.conf`
   following the speech-dispatcher Piper instructions (see the references below). An audio player
   is required (`pw-play` for PipeWire, `paplay` for PulseAudio, `aplay` for ALSA).
3. Restart speech-dispatcher, confirm the module appears in `intone voices list`, then select it:

   ```console
   $ intone config module piper      # use the module name your config defined
   $ intone voices list              # voices now reflect Piper
   $ intone config voice <name>
   ```

Piper still uses espeak-ng internally for text-to-phoneme conversion, so keep espeak-ng
installed.

> The original `rhasspy/piper` repo was archived (read-only) in October 2025; active development
> moved to **`OHF-Voice/piper1-gpl`** (GPL-3.0). Use that for current models and instructions.

### Other modules

Any speech-dispatcher module you install (e.g. **mbrola** voices for espeak, festival) appears in
`intone voices list` and can be selected the same way. intone is deliberately engine-agnostic.

## Platform support

The above is fullest on **Linux** (speech-dispatcher), where rate/pitch/volume, voice/module/
language selection, cycling, and language/context auto-switching all apply live.

- **macOS** (AVFoundation): applies rate, **pitch**, volume, and selects the voice by **identifier**
  (e.g. `com.apple.voice.compact.en-US.Samantha`) or by `language`.
- **Windows** (SAPI): applies rate and volume.

Still to come on those platforms (need real-hardware verification): selecting a voice by *friendly
name* (macOS) and any voice-by-name selection on Windows (SAPI token enumeration), plus the live
per-language/context switching that Linux has.

## References

- speech-dispatcher configuration & Piper generic module — [ArchWiki: Speech dispatcher](https://wiki.archlinux.org/title/Speech_dispatcher)
- Piper (neural TTS, OSS) — [OHF-Voice/piper1-gpl](https://github.com/OHF-Voice/piper1-gpl) · community setup notes: [Piper as a speech-dispatcher plugin](https://github.com/rhasspy/piper/discussions/328)
- espeak-ng — [espeak-ng/espeak-ng](https://github.com/espeak-ng/espeak-ng)
