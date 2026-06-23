# Voices & speech configuration

oxeye speaks through **speech-dispatcher** on Linux, which is *engine-agnostic*: it fronts
multiple text-to-speech **output modules** (espeak-ng, Piper, mbrola, …), each exposing one or
more **voices**. oxeye therefore supports any voice your speech-dispatcher install offers — pick
one with the `oxeye config` commands below. The defaults are fully open-source (espeak-ng), and
oxeye never requires a proprietary/cloud engine.

## Configuring speech

All settings persist to the user config file (`oxeye exclusions path` prints its location) and
take effect the next time the reader starts.

| Command | Effect |
|---|---|
| `oxeye config voice <name>` | Set the synthesis voice (engine-specific name). `default` clears it. |
| `oxeye config module <name>` | Set the speech-dispatcher output module (e.g. `espeak-ng`, `piper`). `default` clears it. |
| `oxeye config language <tag>` | Set the language as a BCP-47 tag (e.g. `en`, `es`). `default` clears it. |
| `oxeye config rate <0–100>` | Speaking rate (50 = normal). |
| `oxeye config pitch <0–100>` | Voice pitch (50 = normal). |
| `oxeye config volume <0–100>` | Volume (100 = full). |
| `oxeye config show` | Print the current configuration, including all speech settings. |

For `voice` / `module` / `language`, the literal value `default` reverts to the engine default.
Levels are validated 0–100 (a value above 100 is rejected, not silently clamped).

## Discovering voices

```console
$ oxeye voices list
output modules: espeak-ng
14805 voices across 140 languages — refine with `oxeye voices list --language <tag>`:
  en-GB (105)
  en-US (105)
  …

$ oxeye voices list --language en
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
voices, switch to it first (`oxeye config module <name>`) and re-run `oxeye voices list`.

> `oxeye voices list` queries the speech-dispatcher daemon; if it isn't running, start it once
> (e.g. `spd-say hello`) and retry.

## Output modules (OSS engines)

### espeak-ng — the default

[espeak-ng](https://github.com/espeak-ng/espeak-ng) ships with speech-dispatcher and needs no
setup. It is compact, extremely responsive, and covers 100+ languages. It is the default module,
so oxeye works out of the box with no configuration. Its voices are formant-synthesis (robotic),
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
3. Restart speech-dispatcher, confirm the module appears in `oxeye voices list`, then select it:

   ```console
   $ oxeye config module piper      # use the module name your config defined
   $ oxeye voices list              # voices now reflect Piper
   $ oxeye config voice <name>
   ```

Piper still uses espeak-ng internally for text-to-phoneme conversion, so keep espeak-ng
installed.

> The original `rhasspy/piper` repo was archived (read-only) in October 2025; active development
> moved to **`OHF-Voice/piper1-gpl`** (GPL-3.0). Use that for current models and instructions.

### Other modules

Any speech-dispatcher module you install (e.g. **mbrola** voices for espeak, festival) appears in
`oxeye voices list` and can be selected the same way. oxeye is deliberately engine-agnostic.

## References

- speech-dispatcher configuration & Piper generic module — [ArchWiki: Speech dispatcher](https://wiki.archlinux.org/title/Speech_dispatcher)
- Piper (neural TTS, OSS) — [OHF-Voice/piper1-gpl](https://github.com/OHF-Voice/piper1-gpl) · community setup notes: [Piper as a speech-dispatcher plugin](https://github.com/rhasspy/piper/discussions/328)
- espeak-ng — [espeak-ng/espeak-ng](https://github.com/espeak-ng/espeak-ng)
