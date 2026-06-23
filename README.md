# intone

> A free, open-source, **cross-platform**, **privacy-respecting** screen reader — built
> core-first so the same engine carries Linux → Windows → macOS. The name: to *intone* is to
> speak in a clear, measured voice — exactly what a screen reader does.

**Status:** `0.1.0` — Linux (AT-SPI2) and Windows (UI Automation) back-ends on a shared Rust
core. See the [`CHANGELOG`](CHANGELOG.md), [`FEASIBILITY.md`](FEASIBILITY.md), and
[`LINUX-FIRST-PLAN.md`](LINUX-FIRST-PLAN.md).

## What makes it different

- **No tracking, ever.** No telemetry, no accounts, no cloud by default. Any network feature
  is explicit, individually toggleable opt-in. Verifiable because it's open source
  (reproducible builds).
- **User-defined exclusions.** Tell the reader to ignore noisy regions, chatty apps, or
  specific controls — by app / role / accessible-name regex / region. Human-readable,
  shareable rules.
- **Open, sandboxed extensibility.** A documented add-on/scripting API (the thing locked away
  in JAWS and absent in Narrator), so the community can extend behavior per app/site.
- **Concise, sane defaults** with granular verbosity control.
- **Cross-platform by construction.** A reusable Rust core on an AccessKit/AT-SPI model;
  Linux first (KDE/Wayland verified), Windows (UIA) and macOS (AXAPI) as later iterations.

## License

**Dual-licensed: [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE), at your option.**

Chosen to be **as permissive as possible** and to **maximize reuse and extensibility across
every platform** — the standard for foundational cross-platform Rust projects (including
[AccessKit](https://github.com/AccessKit/accesskit), a likely dependency):

- **MIT** — maximally permissive, minimal obligations.
- **Apache-2.0** — adds an explicit **patent grant**, so companies and contributors can adopt
  and extend without patent risk.
- **"MIT OR Apache-2.0"** lets each downstream consumer pick whichever they prefer.

### Keeping the permissive promise (dependency-license strategy)

The Linux speech/braille stack is copyleft; it's kept **at arm's length** so no copyleft
reaches intone's code (license-compliance — see the project ruleset):

| Dependency | License | How we use it | Effect |
|------------|---------|---------------|--------|
| speech-dispatcher / eSpeak NG | GPL / LGPL / GPLv3 | **IPC** (separate process, SSIP socket) | none — not linked |
| liblouis (braille, later) | LGPL | **dynamic link** | LGPL permits this from a permissive app |
| AccessKit | MIT OR Apache-2.0 | linked | compatible |

Do **not** statically link or vendor GPL code into the core.

> **Setup note:** `LICENSE-APACHE` must contain the canonical Apache-2.0 text. Populate it
> authoritatively with:
> `curl -fsSL https://www.apache.org/licenses/LICENSE-2.0.txt -o LICENSE-APACHE`

## Architecture (intended)

```
intone-core   — reusable, platform-agnostic: command model, settings, exclusions engine,
               verbosity/announcement policy, scripting host, speech/braille routing
intone-cli    — `intone` command: manage configuration (exclusion rules) — platform-agnostic
intone-linux  — AT-SPI2 tree reader + KWin a11y KeyboardMonitor input (Wayland verified);
               speech-dispatcher output
intone-windows — UI Automation (UIA) back-end (scaffold): focus reading via the shared core;
               compiled in CI on a Windows runner (runtime needs a real desktop)
(later) intone-macos (AXAPI)
```

The Windows back-end reuses `intone-core`'s announcement/exclusions/navigation/braille policy
**unchanged** — it only adapts the UIA tree, events, and output. That core reuse is the whole
point of the core-first design.

## Verified target environment

Parrot OS 7 "Echo", KDE Plasma 6 / KWin Wayland: AT-SPI2 tree access works; global key
capture available via KWin's `org.freedesktop.a11y.KeyboardMonitor`; speech engine needs
install (`speech-dispatcher` + `espeak-ng`). Details in `LINUX-FIRST-PLAN.md`.

## Running

```sh
cargo run -p intone-linux                      # speak (needs audio + speech-dispatcher)
INTONE_SPEECH=text cargo run -p intone-linux    # print announcements (headless/remote dev)
```

Developing remotely and want to *hear* it? Either use `INTONE_SPEECH=text`, or route the audio
to your machine over SSH/tailnet — see [`docs/remote-audio.md`](docs/remote-audio.md)
(`scripts/remote-audio.sh` automates it).

## Managing exclusions

Exclusions tell the reader to ignore, shorten, or de-prioritise announcements from noisy apps,
regions, or controls. Manage them with the `intone` command (writes the user config — no need to
hand-edit TOML):

```sh
intone exclusions list
intone exclusions add --app slack --action suppress              # silence an app
intone exclusions add --name-regex '(?i)cookie' --role banner --action summarize
intone exclusions add --role statusbar --action lower-priority   # speak, but don't interrupt
intone exclusions remove 2                                       # by number from `list`
intone exclusions path                                           # where the config lives
```

A rule matches when **all** its set fields match; the **first** matching rule wins. Actions:
`suppress` (don't announce), `summarize` (first line, length-capped), `lower-priority`
(announce without cutting off current speech). A rule with no matchers is rejected (it would
match everything), and an invalid `--name-regex` fails closed without being saved. The rules are
plain TOML — human-readable and shareable.

## Verbosity

How much detail the reader speaks for each focused element:

```sh
intone config show                  # current verbosity / network / rule count
intone config verbosity low         # label + state/value only
intone config verbosity medium      # adds the role (default)
intone config verbosity high        # adds description + owning application
```

Notable **states** are always spoken (e.g. *checked* / *not checked*, *expanded* / *collapsed*,
*selected*, *dimmed*, *required*, *has popup*), as is a widget's **value** when available — a
slider's number or a single-line text field's content (**never a password field's**, and
multi-line documents aren't dumped on focus). These carry meaning, so they aren't trimmed even
at low verbosity.

As you move the **caret** within a text field, intone announces the character you cross (or the
word/line on a larger jump); **deleting** speaks the removed text, and **selecting** speaks the
selected text (or a length for large selections) — but **never** within a password field.

Press **Ctrl+Alt+S** to hear a summary of the focused application's structure — e.g.
*"3 headings, 12 buttons, 4 links"* — to get oriented in an unfamiliar window.

Move through it by element **type** with a virtual cursor (it starts wherever focus is):
**Ctrl+Alt+H** next heading, **B** next button, **L** next link, **F** next form field — add
**Shift** for the previous one. The target is announced (e.g. *"Save, button"*); at the end you
hear *"no next button"*.

## Voices & speech

Pick the voice and tune the speech — intone uses any voice your speech-dispatcher install offers
(open-source by default: **espeak-ng**, with **Piper** for neural voices):

```sh
intone voices list                  # installed modules + a per-language voice summary
intone voices list --language en    # voices for a language (prefix match)
intone config voice <name>          # select a voice (default = engine default)
intone config module piper          # switch output module (e.g. espeak-ng / piper)
intone config rate 60               # rate / pitch / volume, 0–100
intone config rotation Alan Klaus   # voices to cycle with Ctrl+Alt+V while running
intone config voice-lang es Pedro   # auto-switch voice by the content's language
```

Full guide, including Piper setup: [`docs/voices.md`](docs/voices.md).

## Braille

Enable braille with `intone config braille on`. Each announcement is then also translated to
uncontracted (Grade 1) Unicode braille and emitted, e.g. `[braille] ⠓⠑⠇⠇⠕`. Contracted (Grade 2)
braille and other languages via **liblouis**, and output to a physical display via **BrlAPI**,
slot in behind this translation seam and are planned. The role is treated as chrome
and appears from medium up; the accessible **description** and owning application are extra
context spoken only at high.

`summarize`/`lower-priority` exclusion rules compose with verbosity (a `summarize` rule always
shortens; `lower-priority` keeps the verbosity-appropriate text but doesn't interrupt).
