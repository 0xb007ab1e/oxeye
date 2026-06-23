//! oxeye-linux — Phase-0 spike.
//!
//! Proves the Linux seam end to end on KDE Plasma / Wayland:
//! - **AT-SPI2** focus events → read the focused element's name + role,
//! - `oxeye-core`'s **exclusions** policy decides whether to announce,
//! - output via a persistent **speech-dispatcher (SSIP)** connection that **interrupts** the
//!   previous utterance on each new focus — or, with `OXEYE_SPEECH=text`, printed to stdout,
//! - global **hotkeys** via KWin's `org.freedesktop.a11y.KeyboardMonitor`: **Control**
//!   silences, **Pause** repeats the last announcement.
//!
//! Output mode is chosen by the `OXEYE_SPEECH` env var: `speech` (default), `text`
//! (print only — no audio, no daemon; for headless/remote testing), or `both`.
//!
//! ```text
//! cargo run -p oxeye-linux                 # speak (needs audio + speech-dispatcher)
//! OXEYE_SPEECH=text cargo run -p oxeye-linux   # print announcements (headless/remote)
//! ```

use std::collections::HashMap;

use anyhow::{Context as _, Result};
use atspi::connection::AccessibilityConnection;
use atspi::events::object::{
    StateChangedEvent, TextCaretMovedEvent, TextChangedEvent, TextSelectionChangedEvent,
};
use atspi::events::EventProperties;
use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::cache::CacheProxy;
use atspi::proxy::text::TextProxy;
use atspi::proxy::value::ValueProxy;
use atspi::{Event, Interface, ObjectEvents, Operation, State, StateSet};
use futures_lite::stream::StreamExt;
use ssip_client_async::fifo::asynchronous_tokio::Builder as SsipBuilder;
use ssip_client_async::tokio::AsyncClient;
use ssip_client_async::{ClientError, ClientName, ClientScope, MessageScope};
use tokio::io::{AsyncBufRead, AsyncWrite};

use oxeye_core::announcement;
use oxeye_core::braille;
use oxeye_core::exclusions::{Context as ExclusionContext, ExclusionEngine};
use oxeye_core::navigation::{self, Direction, NavCategory};
use oxeye_core::{Settings, Speech};

/// X keysyms for the keys we react to.
const KEYSYM_CONTROL_L: u32 = 0xffe3;
const KEYSYM_CONTROL_R: u32 = 0xffe4;
const KEYSYM_PAUSE: u32 = 0xff13;
const KEYSYM_O: u32 = 0x6f;
const KEYSYM_S: u32 = 0x73;
const KEYSYM_H: u32 = 0x68;
const KEYSYM_B: u32 = 0x62;
const KEYSYM_L: u32 = 0x6c;
const KEYSYM_F: u32 = 0x66;

/// X11 modifier-mask bits as reported in the `KeyEvent` `state` field.
const MOD_SHIFT: u32 = 0x01;
const MOD_CONTROL: u32 = 0x04;
const MOD_ALT: u32 = 0x08;

/// The concrete SSIP client produced by the tokio fifo builder.
type SsipClient = AsyncClient<
    tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::io::BufWriter<tokio::net::unix::OwnedWriteHalf>,
>;

/// How announcements are emitted.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpeechMode {
    /// Speak via speech-dispatcher only.
    Speech,
    /// Print to stdout only (no audio, no daemon) — for headless/remote testing.
    Text,
    /// Both speak and print.
    Both,
}

impl SpeechMode {
    fn from_env() -> Self {
        match std::env::var("OXEYE_SPEECH").as_deref() {
            Ok("text") => Self::Text,
            Ok("both") => Self::Both,
            _ => Self::Speech,
        }
    }
    fn wants_audio(self) -> bool {
        matches!(self, Self::Speech | Self::Both)
    }
    fn wants_text(self) -> bool {
        matches!(self, Self::Text | Self::Both)
    }
}

/// A braille output channel. Adapters render an announcement's text however their transport
/// needs; this port lets a physical-display adapter (BrlAPI) drop in beside the text sink.
/// See `docs/braille-transport.md`.
trait BrailleSink {
    /// Present `text` on the braille channel.
    fn show(&mut self, text: &str);
}

/// Braille sink that translates to uncontracted (Grade 1) cells and prints them — the dev/
/// remote channel. A device sink (BrlAPI) would instead send the raw text and let BRLTTY
/// translate; see `docs/braille-transport.md`.
struct TextBrailleSink;

impl BrailleSink for TextBrailleSink {
    fn show(&mut self, text: &str) {
        println!("[braille] {}", braille::to_braille(text));
    }
}

/// Output sink for announcements: text, speech, or both — plus an optional braille sink.
struct Speaker {
    mode: SpeechMode,
    client: Option<SsipClient>,
    /// When set, each announcement is also presented on this braille channel.
    braille: Option<Box<dyn BrailleSink>>,
}

impl Speaker {
    /// Announce `text`. When `interrupt` is true (the normal case) any in-progress speech is
    /// cancelled first; when false (a `LowerPriority` exclusion) the announcement is appended
    /// without cutting off what is already being spoken.
    async fn announce(&mut self, text: &str, interrupt: bool) {
        if self.mode.wants_text() {
            if interrupt {
                println!("[say] {text}");
            } else {
                println!("[say:low] {text}");
            }
        }
        if let Some(sink) = self.braille.as_mut() {
            sink.show(text);
        }
        if let Some(client) = self.client.as_mut() {
            if interrupt {
                // CANCEL returns a reply (`210`); consume it so the SPEAK exchange below
                // reads its own responses rather than the cancel's.
                if client.cancel(MessageScope::All).await.is_ok() {
                    let _ = client.receive().await;
                }
            }
            if let Err(err) = say(client, text).await {
                tracing::debug!(%err, "speech failed");
            }
        }
    }

    /// Stop any in-progress speech.
    async fn silence(&mut self) {
        if self.mode.wants_text() {
            println!("[silence]");
        }
        if let Some(client) = self.client.as_mut() {
            if client.cancel(MessageScope::All).await.is_ok() {
                let _ = client.receive().await;
            }
        }
    }
}

/// KWin's accessibility keyboard monitor — the sanctioned global-key path on Wayland.
#[zbus::proxy(
    interface = "org.freedesktop.a11y.KeyboardMonitor",
    default_service = "org.freedesktop.a11y.Manager",
    default_path = "/org/freedesktop/a11y/Manager"
)]
trait KeyboardMonitor {
    /// Receive key events for all keys without consuming them (pass-through).
    fn watch_keyboard(&self) -> zbus::Result<()>;
    /// Stop watching the keyboard.
    fn unwatch_keyboard(&self) -> zbus::Result<()>;
    /// Grab specific key combinations so they are *consumed* (not delivered to the focused
    /// app). `modifiers` lists modifier keysyms; `keystrokes` is `(keysym, modifier_mask)`.
    fn set_key_grabs(&self, modifiers: Vec<u32>, keystrokes: Vec<(u32, u32)>) -> zbus::Result<()>;

    /// Emitted on each key press/release while watching or for grabbed keys.
    #[zbus(signal)]
    fn key_event(
        &self,
        released: bool,
        state: u32,
        keysym: u32,
        unichar: u32,
        keycode: u16,
    ) -> zbus::Result<()>;
}

/// The accessibility status flags; setting `ScreenReaderEnabled` plus owning Orca's
/// well-known name is what authorises us to use KWin's `KeyboardMonitor`.
#[zbus::proxy(
    interface = "org.a11y.Status",
    default_service = "org.a11y.Bus",
    default_path = "/org/a11y/bus"
)]
trait A11yStatus {
    /// Declare that a screen reader is active.
    #[zbus(property)]
    fn set_screen_reader_enabled(&self, value: bool) -> zbus::Result<()>;
    /// Declare that accessibility is enabled.
    #[zbus(property)]
    fn set_is_enabled(&self, value: bool) -> zbus::Result<()>;
}

/// Session-bus pieces needed for hotkeys, kept alive for the run and for clean shutdown.
struct Keyboard {
    session: zbus::Connection,
    a11y_status: A11yStatusProxy<'static>,
    proxy: KeyboardMonitorProxy<'static>,
}

/// The key-grab request oxeye sends KWin: `(modifiers, keystrokes)` for `SetKeyGrabs`.
///
/// `modifiers` is deliberately **empty**. A keysym listed there is *consumed* by the
/// compositor — KWin's `a11ykeyboardmonitor.cpp` `processKey` returns `true` (intercepts) for
/// any key in that list — so registering Control/Alt there swallows every Control and Alt
/// press before the focused application ever sees it, killing all Ctrl/Alt shortcuts and
/// effectively locking the keyboard. We only need to *consume* the specific Ctrl+Alt+<letter>
/// combos, which is exactly what `keystrokes` does. Bare Control still reaches us via
/// `watch_keyboard` pass-through (emitted, but `processKey` returns `false`) for the "silence"
/// hotkey while continuing on to the app.
fn key_grab_spec() -> (Vec<u32>, Vec<(u32, u32)>) {
    let ctrl_alt = MOD_CONTROL | MOD_ALT;
    let ctrl_alt_shift = MOD_CONTROL | MOD_ALT | MOD_SHIFT;
    let keystrokes = vec![
        (KEYSYM_O, ctrl_alt),
        (KEYSYM_S, ctrl_alt),
        (KEYSYM_H, ctrl_alt),
        (KEYSYM_H, ctrl_alt_shift),
        (KEYSYM_B, ctrl_alt),
        (KEYSYM_B, ctrl_alt_shift),
        (KEYSYM_L, ctrl_alt),
        (KEYSYM_L, ctrl_alt_shift),
        (KEYSYM_F, ctrl_alt),
        (KEYSYM_F, ctrl_alt_shift),
    ];
    (Vec::new(), keystrokes)
}

#[cfg(test)]
mod key_grab_tests {
    use super::{key_grab_spec, KEYSYM_S, MOD_ALT, MOD_CONTROL};

    #[test]
    fn never_grabs_standalone_modifier_keys() {
        let (modifiers, keystrokes) = key_grab_spec();
        // Regression guard: a non-empty `modifiers` list makes KWin consume those bare keys,
        // swallowing every Ctrl/Alt shortcut before the app sees it (the keyboard lockup).
        assert!(
            modifiers.is_empty(),
            "must not grab standalone modifier keys"
        );
        // The dedicated hotkeys are still consumed, but only as full Ctrl+Alt combos.
        let ctrl_alt = MOD_CONTROL | MOD_ALT;
        assert!(keystrokes.contains(&(KEYSYM_S, ctrl_alt)));
        assert_eq!(keystrokes.len(), 10);
        assert!(
            keystrokes.iter().all(|(_, m)| m & ctrl_alt == ctrl_alt),
            "every grabbed keystroke carries Ctrl+Alt, never a bare key"
        );
    }
}

/// Best-effort hotkey setup: declare a screen reader is active, claim the well-known name
/// KWin requires, and start watching keys. Returns `None` (rather than erroring) when the
/// compositor doesn't provide `KeyboardMonitor`, so focus readout still works on non-KWin
/// or headless sessions.
async fn setup_keyboard() -> Option<Keyboard> {
    let session = zbus::Connection::session().await.ok()?;
    let a11y_status = A11yStatusProxy::new(&session).await.ok()?;
    let _ = a11y_status.set_is_enabled(true).await;
    let _ = a11y_status.set_screen_reader_enabled(true).await;
    // KWin 6.3.x authorises KeyboardMonitor *only* for the owner of Orca's well-known name
    // (hardcoded in `a11ykeyboardmonitor.cpp`).
    session
        .request_name("org.gnome.Orca.KeyboardMonitor")
        .await
        .ok()?;
    let proxy = KeyboardMonitorProxy::new(&session).await.ok()?;
    proxy.watch_keyboard().await.ok()?;
    // Grab dedicated, *consumed* shortcuts (won't reach the focused app): Ctrl+Alt+O (time),
    // Ctrl+Alt+S (structure summary), and Ctrl+Alt+{H,B,L,F} to move to the next element of a
    // type (heading/button/link/form field); add Shift for the previous one. The standalone-
    // modifier list stays empty on purpose — see `key_grab_spec`.
    let (modifiers, grabs) = key_grab_spec();
    let _ = proxy.set_key_grabs(modifiers, grabs).await;
    Some(Keyboard {
        session,
        a11y_status,
        proxy,
    })
}

/// Run the Linux screen-reader back-end: connect AT-SPI, speech, and hotkeys, then loop
/// until interrupted. The `oxeye-linux` binary sets up the async runtime and calls this.
pub async fn run() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mode = SpeechMode::from_env();
    let settings = Settings::load().unwrap_or_default();
    let exclusions = ExclusionEngine::compile(&settings.exclusions).unwrap_or_default();

    // Speech: only connect (and possibly auto-spawn the daemon) when audio is wanted.
    let client = if mode.wants_audio() {
        Some(connect_speech(&settings).await?)
    } else {
        None
    };
    let braille: Option<Box<dyn BrailleSink>> = settings
        .braille
        .then(|| Box::new(TextBrailleSink) as Box<dyn BrailleSink>);
    let mut speaker = Speaker {
        mode,
        client,
        braille,
    };

    // Accessibility: subscribe to focus changes.
    let conn = AccessibilityConnection::new()
        .await
        .context("connecting to the AT-SPI accessibility bus")?;
    conn.register_event::<StateChangedEvent>()
        .await
        .context("registering for state-changed events")?;
    conn.register_event::<TextCaretMovedEvent>()
        .await
        .context("registering for caret-moved events")?;
    conn.register_event::<TextChangedEvent>()
        .await
        .context("registering for text-changed events")?;
    conn.register_event::<TextSelectionChangedEvent>()
        .await
        .context("registering for text-selection-changed events")?;
    let atspi_events = conn.event_stream();
    futures_lite::pin!(atspi_events);

    // Hotkeys are best-effort: if the compositor doesn't offer KeyboardMonitor (non-KWin,
    // headless, or unauthorised), continue with focus readout only — essential for
    // OXEYE_SPEECH=text on machines without a KWin/Wayland session.
    let keyboard = setup_keyboard().await;
    if keyboard.is_none() {
        tracing::warn!(
            "hotkeys unavailable (no KWin KeyboardMonitor); continuing with focus readout only"
        );
    }
    let mut key_events: std::pin::Pin<Box<dyn futures_lite::stream::Stream<Item = KeyEvent> + '_>> =
        match &keyboard {
            Some(kb) => Box::pin(kb.proxy.receive_key_event().await?),
            None => Box::pin(futures_lite::stream::pending()),
        };
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing SIGTERM handler")?;

    eprintln!(
        "oxeye spike ({}): Tab/Alt-Tab to hear focus · Control silences · Pause repeats · Ctrl-C quits.",
        match mode {
            SpeechMode::Speech => "speech",
            SpeechMode::Text => "text",
            SpeechMode::Both => "speech+text",
        }
    );
    speaker.announce("oxeye spike running", true).await;

    let mut last_text: Option<String> = None;
    let mut caret: Option<CaretTracker> = None;
    // Bus name of the most recently focused application, for the structure-summary hotkey.
    let mut focused_app: Option<String> = None;
    // Virtual navigation cursor (sender, path) for by-type movement; follows focus.
    let mut nav_cursor: Option<(String, String)> = None;

    loop {
        tokio::select! {
            Some(event) = atspi_events.next() => {
                let Ok(event) = event else { continue };
                match event {
                    Event::Object(ObjectEvents::StateChanged(state)) => {
                        if state.state != State::Focused || !state.enabled {
                            continue;
                        }
                        let focused = match read_focus(&conn, &state).await {
                            Ok(focused) => focused,
                            Err(err) => {
                                tracing::debug!(%err, "could not describe focused element");
                                continue;
                            }
                        };
                        focused_app = Some(state.sender().to_string());
                        nav_cursor = Some((state.sender().to_string(), state.path().to_string()));
                        // Track the caret for editable text objects, remembering whether it is a
                        // password field so caret moves there never echo characters.
                        caret = focused.has_text.then(|| CaretTracker {
                            sender: state.sender().to_string(),
                            path: state.path().to_string(),
                            password: focused.role == "password text",
                            last: None,
                            pending_insert: None,
                            suppress_caret_at: None,
                        });
                        tracing::debug!(
                            app = %focused.app,
                            role = %focused.role,
                            has_text = focused.has_text,
                            "focused element (caret tracking when has_text)"
                        );
                        let ctx = ExclusionContext {
                            app: &focused.app,
                            role: &focused.role,
                            name: &focused.name,
                        };
                        let action = exclusions.evaluate(&ctx);
                        let element = announcement::Element {
                            ident: ctx,
                            description: &focused.description,
                            value: focused.value.as_deref(),
                            states: focused.states,
                        };
                        let composed =
                            announcement::compose(&element, settings.verbosity, action);
                        let Some(ann) = composed else {
                            continue; // suppressed by an exclusion rule
                        };
                        last_text = Some(ann.text.clone());
                        speaker.announce(&ann.text, ann.interrupt).await;
                    }
                    Event::Object(ObjectEvents::TextCaretMoved(moved)) => {
                        tracing::debug!(pos = moved.position, "text-caret-moved event");
                        let Some(tracker) = caret.as_mut() else { continue };
                        if moved.sender().to_string() != tracker.sender
                            || moved.path().to_string() != tracker.path
                        {
                            continue; // a caret move on some other (stale) object
                        }
                        let new = moved.position;
                        let pending_insert = tracker.pending_insert.take();
                        // Suppress the caret move a deletion triggers (the removed text was
                        // announced by the text-changed handler instead).
                        let suppressed = tracker.suppress_caret_at.take() == Some(new);
                        let spoken = match caret_action(
                            tracker.password,
                            pending_insert,
                            suppressed,
                            tracker.last,
                        ) {
                            // A typed/pasted insertion: echo the inserted text read straight from
                            // the field (robust even for the first event after focus).
                            CaretAction::Inserted { start } => {
                                read_inserted_text(&conn, &moved, start, new).await
                            }
                            // A bare navigation move: echo the traversed character/word/line.
                            CaretAction::Moved { from } => {
                                read_caret_text(&conn, &moved, from, new).await
                            }
                            CaretAction::Nothing => None,
                        };
                        tracker.last = Some(new);
                        if let Some(text) = spoken {
                            last_text = Some(text.clone());
                            speaker.announce(&text, true).await;
                        }
                    }
                    Event::Object(ObjectEvents::TextChanged(changed)) => {
                        tracing::debug!(op = ?changed.operation, "text-changed event");
                        let Some(tracker) = caret.as_mut() else { continue };
                        if changed.sender().to_string() != tracker.sender
                            || changed.path().to_string() != tracker.path
                        {
                            continue;
                        }
                        match changed.operation {
                            // Mark the insertion; the paired caret move reads and echoes the new
                            // character(s) from the field itself (so even the first one speaks).
                            Operation::Insert => {
                                tracker.pending_insert = Some(changed.start_pos);
                            }
                            // Announce the removed text here and suppress the caret move it fires.
                            Operation::Delete => {
                                tracker.suppress_caret_at = Some(changed.start_pos);
                                if !tracker.password {
                                    if let Some(text) = describe_change(&changed.text) {
                                        last_text = Some(text.clone());
                                        speaker.announce(&text, true).await;
                                    }
                                }
                            }
                        }
                    }
                    Event::Object(ObjectEvents::TextSelectionChanged(sel)) => {
                        let Some(tracker) = caret.as_mut() else { continue };
                        if sel.sender().to_string() != tracker.sender
                            || sel.path().to_string() != tracker.path
                        {
                            continue;
                        }
                        let Some(read) = read_selection(&conn, &sel).await else {
                            continue;
                        };
                        // A non-empty selection moves the caret to its active end; suppress that
                        // paired caret move (we announce the selection instead). A cleared
                        // selection leaves its caret move as normal navigation.
                        if read.text.is_some() {
                            if let Some(offset) = read.caret {
                                tracker.suppress_caret_at = Some(offset);
                            }
                        }
                        if tracker.password {
                            continue; // never reveal a password field's content
                        }
                        if let Some(text) = read.text {
                            last_text = Some(text.clone());
                            speaker.announce(&text, true).await;
                        }
                    }
                    _ => {}
                }
            }
            Some(signal) = key_events.next() => {
                let Ok(args) = signal.args() else { continue };
                if args.released {
                    continue; // act on press only
                }
                match args.keysym {
                    KEYSYM_CONTROL_L | KEYSYM_CONTROL_R => speaker.silence().await,
                    KEYSYM_PAUSE => {
                        let text = last_text
                            .clone()
                            .unwrap_or_else(|| "nothing focused yet".to_owned());
                        speaker.announce(&text, true).await;
                    }
                    KEYSYM_O if has_ctrl_alt(args.state) => {
                        speaker
                            .announce(&format!("time, {}", current_time()), true)
                            .await;
                    }
                    KEYSYM_S if has_ctrl_alt(args.state) => {
                        let summary = match &focused_app {
                            Some(app) => summarize_structure(&conn, app).await,
                            None => None,
                        };
                        let text = summary
                            .unwrap_or_else(|| "no structure to summarize".to_owned());
                        speaker.announce(&text, true).await;
                    }
                    KEYSYM_H | KEYSYM_B | KEYSYM_L | KEYSYM_F
                        if has_ctrl_alt(args.state) =>
                    {
                        let target = match args.keysym {
                            KEYSYM_H => NavCategory::Heading,
                            KEYSYM_B => NavCategory::Button,
                            KEYSYM_L => NavCategory::Link,
                            _ => NavCategory::FormField,
                        };
                        let moved = navigate_by_category(
                            &conn,
                            focused_app.as_deref(),
                            &mut nav_cursor,
                            target,
                            direction_from(args.state),
                        )
                        .await;
                        if let Some(text) = moved {
                            last_text = Some(text.clone());
                            speaker.announce(&text, true).await;
                        }
                    }
                    _ => {}
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down (SIGINT): releasing screen-reader role");
                break;
            }
            _ = sigterm.recv() => {
                tracing::info!("shutting down (SIGTERM): releasing screen-reader role");
                break;
            }
            else => break,
        }
    }

    // Graceful shutdown: stop watching keys, release the Orca name, and clear the a11y
    // flags so the desktop doesn't stay in "screen reader active" state after we exit.
    if let Some(kb) = &keyboard {
        let _ = kb.proxy.unwatch_keyboard().await;
        let _ = kb
            .session
            .release_name("org.gnome.Orca.KeyboardMonitor")
            .await;
        let _ = kb.a11y_status.set_screen_reader_enabled(false).await;
    }
    speaker.silence().await;

    Ok(())
}

/// Connect to speech-dispatcher (auto-spawning + polling for its socket), name the client,
/// and apply the configured rate.
async fn connect_speech(settings: &Settings) -> Result<SsipClient> {
    let mut tts = match SsipBuilder::default().build().await {
        Ok(client) => client,
        Err(_) => {
            tracing::info!("speech-dispatcher not reachable; starting it");
            let _ = std::process::Command::new("speech-dispatcher").spawn();
            // A cold-start daemon takes a moment to create its socket: poll, don't guess.
            let mut connected = None;
            for _ in 0..20 {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                if let Ok(client) = SsipBuilder::default().build().await {
                    connected = Some(client);
                    break;
                }
            }
            connected.context(
                "connecting to speech-dispatcher \
                 (install it: `sudo apt install speech-dispatcher espeak-ng`)",
            )?
        }
    };
    tts.set_client_name(ClientName::new("oxeye", "oxeye"))
        .await
        .context("registering SSIP client name")?;
    tts.check_client_name_set()
        .await
        .context("confirming SSIP client name")?;
    apply_speech_settings(&mut tts, &settings.speech).await;
    Ok(tts)
}

/// Apply rate/pitch/volume/voice/language/output-module from settings (best-effort).
async fn apply_speech_settings(tts: &mut SsipClient, speech: &Speech) {
    // Every SET writes a request whose reply (`208`-class OK) must be consumed with `receive`
    // so the response stream stays in step for the SPEAK exchange that follows. Best-effort:
    // a failed write is skipped (and leaves no reply to read).
    if let Some(module) = &speech.output_module {
        if tts
            .set_output_module(ClientScope::Current, module)
            .await
            .is_ok()
        {
            let _ = tts.receive().await;
        }
    }
    if let Some(voice) = &speech.voice {
        if tts
            .set_synthesis_voice(ClientScope::Current, voice)
            .await
            .is_ok()
        {
            let _ = tts.receive().await;
        }
    }
    if let Some(lang) = &speech.language {
        if tts.set_language(ClientScope::Current, lang).await.is_ok() {
            let _ = tts.receive().await;
        }
    }
    if tts
        .set_rate(ClientScope::Current, to_ssip_scale(speech.rate))
        .await
        .is_ok()
    {
        let _ = tts.receive().await;
    }
    if tts
        .set_pitch(ClientScope::Current, to_ssip_scale(speech.pitch))
        .await
        .is_ok()
    {
        let _ = tts.receive().await;
    }
    if tts
        .set_volume(ClientScope::Current, to_ssip_scale(speech.volume))
        .await
        .is_ok()
    {
        let _ = tts.receive().await;
    }
}

/// Map a 0..=100 user setting onto SSIP's -100..=100 scale (50 -> 0, 100 -> +100).
fn to_ssip_scale(value: u8) -> i8 {
    (i16::from(value) * 2 - 100).clamp(-100, 100) as i8
}

#[cfg(test)]
mod scale_tests {
    use super::to_ssip_scale;

    #[test]
    fn maps_0_100_onto_ssip_range() {
        assert_eq!(to_ssip_scale(50), 0);
        assert_eq!(to_ssip_scale(0), -100);
        assert_eq!(to_ssip_scale(100), 100);
        assert_eq!(to_ssip_scale(75), 50);
    }
}

/// A focused element described from the accessibility tree, owned for the announcement step.
struct Focused {
    app: String,
    name: String,
    role: String,
    description: String,
    value: Option<String>,
    states: announcement::States,
    /// Whether the object exposes the AT-SPI Text interface (so the caret can be tracked).
    has_text: bool,
}

/// Build an accessible proxy for the event's object and read its describable attributes.
async fn read_focus(conn: &AccessibilityConnection, ev: &StateChangedEvent) -> Result<Focused> {
    let sender = ev.sender().to_string();
    let path = ev.path().to_string();
    let proxy = AccessibleProxy::builder(conn.connection())
        .destination(ev.sender())?
        .path(ev.path())?
        // AT-SPI properties have no PropertiesChanged signal (changes arrive via AT-SPI's own
        // events), so zbus's default caching would serve stale data — and the eager GetAll it
        // issues on build SIGSEGVs Qt's a11y bridge. The atspi crate disables caching for the
        // same reason. See docs/investigations/qt-atspi-caching.md (issue #6).
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .context("building AccessibleProxy")?;
    let name = match proxy.name().await {
        Ok(name) => name,
        Err(err) => {
            tracing::warn!(%err, %sender, %path, "name() failed");
            String::new()
        }
    };
    let role = match proxy.get_role().await {
        Ok(role) => role.name().to_owned(),
        Err(err) => {
            tracing::warn!(%err, %sender, %path, "get_role() failed");
            "element".to_owned()
        }
    };
    // Description and state are best-effort: a failure degrades detail, not the announcement.
    let description = proxy.description().await.unwrap_or_default();
    let state_set = match proxy.get_state().await {
        Ok(set) => set,
        Err(err) => {
            tracing::debug!(%err, %sender, %path, "get_state() failed");
            StateSet::default()
        }
    };
    let states = states_from(state_set);
    // Surface a textual value, querying only interfaces the object advertises (never probing
    // one it lacks). Numeric widgets use the Value interface; editable fields use the Text
    // interface — but never a password field, and not whole multi-line documents.
    let (has_value, has_text) = match proxy.get_interfaces().await {
        Ok(ifaces) => (
            ifaces.contains(Interface::Value),
            ifaces.contains(Interface::Text),
        ),
        Err(_) => (false, false),
    };
    let value = if has_value {
        read_value(conn, ev).await
    } else if has_text && role != "password text" && !state_set.contains(State::MultiLine) {
        read_text(conn, ev).await
    } else {
        None
    };
    let app = read_app_name(conn, &proxy).await;
    if name.is_empty() {
        // Unnamed containers (panels/frames/fillers) are normal; not worth a warning.
        tracing::debug!(%sender, %path, %app, %role, "focused object has no accessible name");
    }
    Ok(Focused {
        app,
        name,
        role,
        description,
        value,
        states,
        has_text,
    })
}

/// Read the current numeric value via the AT-SPI Value interface and format it for speech.
/// Best-effort: returns `None` if the proxy can't be built, the value can't be read, or it is
/// not finite. Caching is **off** (a single property `Get`, never `GetAll` — see issue #6).
async fn read_value(conn: &AccessibilityConnection, ev: &StateChangedEvent) -> Option<String> {
    let proxy = ValueProxy::builder(conn.connection())
        .destination(ev.sender())
        .ok()?
        .path(ev.path())
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    let current = proxy.current_value().await.ok()?;
    current
        .is_finite()
        .then(|| announcement::format_value(current))
}

/// Upper bound on text-field content read for an announcement, in characters. Single-line
/// fields are short; this caps any pathological case and bounds the `GetText` call.
const TEXT_CONTENT_MAX_CHARS: i32 = 200;

/// Read editable text-field content via the AT-SPI Text interface, bounded in length. Caching
/// is **off** (per-method `Get`, never `GetAll` — see issue #6). Best-effort: returns `None` on
/// error or when empty. Callers must gate out password and multi-line fields first.
async fn read_text(conn: &AccessibilityConnection, ev: &StateChangedEvent) -> Option<String> {
    let proxy = TextProxy::builder(conn.connection())
        .destination(ev.sender())
        .ok()?
        .path(ev.path())
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    let count = proxy.character_count().await.ok()?;
    if count <= 0 {
        return None;
    }
    let raw = proxy
        .get_text(0, count.min(TEXT_CONTENT_MAX_CHARS))
        .await
        .ok()?;
    clean_text(&raw)
}

// Numeric value formatting lives in `oxeye_core::announcement::format_value` (shared with the
// Windows back-end).

/// Trim text-field content for speech and drop it if there is nothing left.
fn clean_text(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// What a caret move should announce, decided purely from tracker state; the (async) text reads
/// follow from the variant.
#[derive(Debug, PartialEq, Eq)]
enum CaretAction {
    /// A text insertion starting at this offset — read and echo the inserted run.
    Inserted { start: i32 },
    /// A bare navigation move from this previous offset — read and echo the traversed text.
    Moved { from: i32 },
    /// Nothing to announce: a password field, a deletion's paired move, or the first bare move
    /// after focus (which only establishes the baseline).
    Nothing,
}

/// Decide what a caret move announces from the tracker's state. An **insertion is echoed even
/// with no prior baseline** — the first typed character must speak (the bug this guards). A
/// password never echoes; a deletion's paired move is suppressed; an unbaselined bare move is
/// silent (it just sets the baseline). Pure, so the dispatch is unit-tested without AT-SPI.
fn caret_action(
    password: bool,
    pending_insert: Option<i32>,
    suppressed: bool,
    last: Option<i32>,
) -> CaretAction {
    if password {
        CaretAction::Nothing
    } else if let Some(start) = pending_insert {
        CaretAction::Inserted { start }
    } else if suppressed {
        CaretAction::Nothing
    } else {
        match last {
            Some(from) => CaretAction::Moved { from },
            None => CaretAction::Nothing,
        }
    }
}

#[cfg(test)]
mod caret_action_tests {
    use super::{caret_action, CaretAction};

    #[test]
    fn insertion_is_announced_even_as_the_first_event_after_focus() {
        // Regression: the first typed character was swallowed because the first caret event had
        // no baseline. An insertion must announce regardless of `last`.
        assert_eq!(
            caret_action(false, Some(0), false, None),
            CaretAction::Inserted { start: 0 }
        );
    }

    #[test]
    fn insertion_takes_precedence_over_a_suppressed_move() {
        assert_eq!(
            caret_action(false, Some(2), true, Some(2)),
            CaretAction::Inserted { start: 2 }
        );
    }

    #[test]
    fn password_field_never_echoes() {
        assert_eq!(
            caret_action(true, Some(0), false, Some(3)),
            CaretAction::Nothing
        );
    }

    #[test]
    fn deletions_paired_move_is_suppressed() {
        assert_eq!(caret_action(false, None, true, Some(3)), CaretAction::Nothing);
    }

    #[test]
    fn first_bare_move_after_focus_only_baselines() {
        assert_eq!(caret_action(false, None, false, None), CaretAction::Nothing);
    }

    #[test]
    fn later_bare_move_is_navigation() {
        assert_eq!(
            caret_action(false, None, false, Some(2)),
            CaretAction::Moved { from: 2 }
        );
    }
}

/// Tracks the caret in the currently focused editable text object so caret-moved events can
/// announce the traversed character (or the word/line on a larger jump) relative to the last
/// known position.
struct CaretTracker {
    sender: String,
    path: String,
    /// A password field — caret moves must never echo its characters.
    password: bool,
    /// Last known caret offset; `None` until the first caret move after focus (the baseline).
    last: Option<i32>,
    /// Start offset of a just-seen insertion. The paired caret move reads and echoes the
    /// inserted text (from here to the new caret) straight from the Text interface, so a typed
    /// character is announced even when it is the first event after focus. Consumed on that move.
    pending_insert: Option<i32>,
    /// Offset the caret will land on after a deletion: the matching caret move is suppressed
    /// (the removed text is announced instead). Consumed on the next caret move.
    suppress_caret_at: Option<i32>,
}

/// AT-SPI `TextBoundaryType` granularities for `get_text_at_offset`.
const GRAN_CHAR: u32 = 0;
const GRAN_WORD_START: u32 = 1;
const GRAN_LINE_START: u32 = 5;

/// Read the text inserted between `start` (the insertion point) and `new` (the caret after it)
/// to echo a typed or pasted edit — the whitespace-aware char form for a single character,
/// otherwise the inserted run. Caching is **off** (per-method `Get`, never `GetAll` — see issue
/// #6). Best-effort: `None` on error or an empty/degenerate range.
async fn read_inserted_text(
    conn: &AccessibilityConnection,
    ev: &TextCaretMovedEvent,
    start: i32,
    new: i32,
) -> Option<String> {
    if new <= start {
        return None;
    }
    let proxy = TextProxy::builder(conn.connection())
        .destination(ev.sender())
        .ok()?
        .path(ev.path())
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    if new - start == 1 {
        let (s, _, _) = proxy
            .get_text_at_offset(start.max(0), GRAN_CHAR)
            .await
            .ok()?;
        Some(speak_char(&s))
    } else {
        let inserted = proxy.get_text(start.max(0), new).await.ok()?;
        clean_text(&inserted)
    }
}

/// Read the text to announce for a caret move from `last` to `new`: the single character
/// traversed on a one-step move, otherwise the word at the caret (falling back to the line).
/// Caching is **off** (per-method `Get`, never `GetAll` — see issue #6).
async fn read_caret_text(
    conn: &AccessibilityConnection,
    ev: &TextCaretMovedEvent,
    last: i32,
    new: i32,
) -> Option<String> {
    let proxy = TextProxy::builder(conn.connection())
        .destination(ev.sender())
        .ok()?
        .path(ev.path())
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    match new - last {
        0 => None, // no movement (e.g. caret re-placed at the same offset) — nothing to read
        1 => {
            let (s, _, _) = proxy
                .get_text_at_offset((new - 1).max(0), GRAN_CHAR)
                .await
                .ok()?;
            Some(speak_char(&s))
        }
        -1 => {
            let (s, _, _) = proxy.get_text_at_offset(new.max(0), GRAN_CHAR).await.ok()?;
            Some(speak_char(&s))
        }
        _ => {
            let (word, _, _) = proxy
                .get_text_at_offset(new.max(0), GRAN_WORD_START)
                .await
                .ok()?;
            if let Some(spoken) = clean_text(&word) {
                return Some(spoken);
            }
            let (line, _, _) = proxy
                .get_text_at_offset(new.max(0), GRAN_LINE_START)
                .await
                .ok()?;
            clean_text(&line)
        }
    }
}

/// The result of inspecting a text object's selection: what to announce (if anything) plus the
/// current caret offset (to suppress the caret move the selection triggered).
struct SelectionRead {
    text: Option<String>,
    caret: Option<i32>,
}

/// Read the current text selection via the Text interface. Caching is **off** (per-method
/// `Get`, never `GetAll`). Reads at most [`TEXT_CONTENT_MAX_CHARS`] of selected text; larger
/// selections are reported by length. Returns `None` only if the proxy can't be built.
async fn read_selection(
    conn: &AccessibilityConnection,
    ev: &TextSelectionChangedEvent,
) -> Option<SelectionRead> {
    let proxy = TextProxy::builder(conn.connection())
        .destination(ev.sender())
        .ok()?
        .path(ev.path())
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    let caret = proxy.caret_offset().await.ok();
    if proxy.get_n_selections().await.ok()? <= 0 {
        return Some(SelectionRead { text: None, caret });
    }
    let (start, end) = proxy.get_selection(0).await.ok()?;
    let length = end - start;
    if length <= 0 {
        return Some(SelectionRead { text: None, caret });
    }
    let selected = if length <= TEXT_CONTENT_MAX_CHARS {
        proxy.get_text(start, end).await.unwrap_or_default()
    } else {
        String::new()
    };
    Some(SelectionRead {
        text: format_selection(&selected, length),
        caret,
    })
}

/// Summarise the focused application's structure via the AT-SPI Cache — one `GetItems` call
/// returns the whole tree (role + name), so there is no per-node walk. Caching is **off**;
/// returns `None` if the cache is unavailable or nothing notable is present.
async fn summarize_structure(conn: &AccessibilityConnection, app: &str) -> Option<String> {
    let cache = CacheProxy::builder(conn.connection())
        .destination(app)
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    let items = cache.get_items().await.ok()?;
    let categories = items
        .iter()
        .map(|item| navigation::classify(item.role.name()));
    navigation::summarize(categories)
}

/// Map a key event's modifier state to a navigation [`Direction`] (Shift ⇒ previous).
fn direction_from(state: u32) -> Direction {
    if state & MOD_SHIFT != 0 {
        Direction::Previous
    } else {
        Direction::Next
    }
}

/// A stable id for an accessible object: its owning bus name and object path.
fn object_id(object: &atspi::ObjectRefOwned) -> (String, String) {
    (
        object.name_as_str().unwrap_or_default().to_owned(),
        object.path_as_str().to_owned(),
    )
}

/// Move the virtual navigation cursor to the next/previous element of `target` type and return
/// what to announce. Uses the AT-SPI Cache (one `GetItems`), flattens it into document order in
/// the pure core, and searches from the cursor. Returns `None` only when no application is
/// focused or the cache is unavailable; otherwise `Some` (including "no next/previous …").
async fn navigate_by_category(
    conn: &AccessibilityConnection,
    app: Option<&str>,
    cursor: &mut Option<(String, String)>,
    target: NavCategory,
    direction: Direction,
) -> Option<String> {
    let app = app?;
    let cache = CacheProxy::builder(conn.connection())
        .destination(app)
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    let items = cache.get_items().await.ok()?;
    if items.is_empty() {
        return None;
    }
    let index_of: HashMap<(String, String), usize> = items
        .iter()
        .enumerate()
        .map(|(i, item)| (object_id(&item.object), i))
        .collect();
    let nodes: Vec<navigation::TreeNode> = items
        .iter()
        .map(|item| navigation::TreeNode {
            parent: index_of.get(&object_id(&item.parent)).copied(),
            index_in_parent: item.index,
        })
        .collect();
    let order = navigation::document_order(&nodes);
    let categories: Vec<Option<NavCategory>> = order
        .iter()
        .map(|&pos| navigation::classify(items[pos].role.name()))
        .collect();
    let from = cursor
        .as_ref()
        .and_then(|id| index_of.get(id))
        .and_then(|&item_idx| order.iter().position(|&pos| pos == item_idx));

    match navigation::find(&categories, from, target, direction) {
        Some(found) => {
            let item = &items[order[found]];
            *cursor = Some(object_id(&item.object));
            let name = item.name.trim();
            let label = target.singular();
            Some(if name.is_empty() {
                label.to_owned()
            } else {
                format!("{name}, {label}")
            })
        }
        None => {
            let dir = match direction {
                Direction::Next => "next",
                Direction::Previous => "previous",
            };
            Some(format!("no {dir} {}", target.singular()))
        }
    }
}

/// Spoken form of a selection: the trimmed selected text plus "selected", or a length summary
/// for selections too long to read aloud. Returns `None` when there is nothing meaningful.
fn format_selection(selected: &str, length: i32) -> Option<String> {
    if length > TEXT_CONTENT_MAX_CHARS {
        Some(format!("{length} characters selected"))
    } else {
        clean_text(selected).map(|text| format!("{text} selected"))
    }
}

/// Spoken form of inserted/deleted text from a text-changed event: nothing for an empty
/// change, the whitespace-aware form for a single character, otherwise the trimmed text.
fn describe_change(text: &str) -> Option<String> {
    if text.is_empty() {
        None
    } else if text.chars().count() == 1 {
        Some(speak_char(text))
    } else {
        clean_text(text)
    }
}

/// Spoken form of a single traversed character: whitespace becomes a word; other characters
/// are announced as-is.
fn speak_char(s: &str) -> String {
    match s {
        " " => "space".to_owned(),
        "\t" => "tab".to_owned(),
        "\n" | "\r\n" | "\r" => "new line".to_owned(),
        "" => "blank".to_owned(),
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod value_format_tests {
    use super::{clean_text, describe_change, format_selection, speak_char};

    #[test]
    fn format_selection_announces_text_or_count() {
        assert_eq!(
            format_selection("hello", 5),
            Some("hello selected".to_owned())
        );
        assert_eq!(format_selection("  ", 2), None); // whitespace-only selection
        assert_eq!(
            format_selection("ignored", 500),
            Some("500 characters selected".to_owned())
        );
    }

    #[test]
    fn describe_change_handles_empty_single_and_multi() {
        assert_eq!(describe_change(""), None);
        assert_eq!(describe_change("x"), Some("x".to_owned()));
        assert_eq!(describe_change(" "), Some("space".to_owned()));
        assert_eq!(describe_change("  hello  "), Some("hello".to_owned()));
    }

    #[test]
    fn speak_char_maps_whitespace_to_words() {
        assert_eq!(speak_char("a"), "a");
        assert_eq!(speak_char(" "), "space");
        assert_eq!(speak_char("\t"), "tab");
        assert_eq!(speak_char("\n"), "new line");
        assert_eq!(speak_char(""), "blank");
    }

    #[test]
    fn clean_text_trims_and_drops_empty() {
        assert_eq!(clean_text("  hi  "), Some("hi".to_owned()));
        assert_eq!(clean_text("John"), Some("John".to_owned()));
        assert_eq!(clean_text("   "), None);
        assert_eq!(clean_text(""), None);
    }
}

/// Map an AT-SPI [`StateSet`] onto the subset of states oxeye announces. "Disabled" is gated on
/// focusability so static, non-interactive content isn't reported as dimmed.
fn states_from(set: StateSet) -> announcement::States {
    announcement::States {
        checkable: set.contains(State::Checkable),
        checked: set.contains(State::Checked),
        expandable: set.contains(State::Expandable),
        expanded: set.contains(State::Expanded),
        selected: set.contains(State::Selected),
        disabled: set.contains(State::Focusable) && !set.contains(State::Sensitive),
        required: set.contains(State::Required),
        has_popup: set.contains(State::HasPopup),
    }
}

/// Resolve the accessible *application* name owning `focused`, for per-app exclusion rules.
/// Best-effort: returns an empty string if it can't be determined.
async fn read_app_name(conn: &AccessibilityConnection, focused: &AccessibleProxy<'_>) -> String {
    let Ok(app_ref) = focused.get_application().await else {
        return String::new();
    };
    if app_ref.is_null() {
        return String::new();
    }
    let (Some(name), path) = (app_ref.name(), app_ref.path()) else {
        return String::new();
    };
    let Ok(builder) = AccessibleProxy::builder(conn.connection()).destination(name.clone()) else {
        return String::new();
    };
    let Ok(builder) = builder.path(path.clone()) else {
        return String::new();
    };
    let Ok(app_proxy) = builder
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
    else {
        return String::new();
    };
    app_proxy.name().await.unwrap_or_default()
}

/// True if both Control and Alt are held, per a `KeyEvent` modifier `state`.
fn has_ctrl_alt(state: u32) -> bool {
    let mask = MOD_CONTROL | MOD_ALT;
    (state & mask) == mask
}

/// The current local time as a short spoken string (e.g. "3:07 PM"), via `date`.
fn current_time() -> String {
    std::process::Command::new("date")
        .arg("+%-I:%M %p")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| "unavailable".to_owned())
}

/// Send one utterance over SSIP and read back its message id.
async fn say<R, W>(tts: &mut AsyncClient<R, W>, text: &str) -> Result<(), ClientError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // SSIP is strict request/response: each step must read its reply or the stream desyncs.
    // SPEAK → `230 RECEIVING DATA` → the text lines (terminated by a lone `.`, which
    // `send_lines` appends) → `225 MESSAGE QUEUED` + id. `send_line` alone neither consumes
    // the 230 nor sends the terminating dot, so the message would never queue.
    tts.speak().await?;
    tts.check_receiving_data().await?;
    tts.send_lines(&[text.to_owned()]).await?;
    tts.receive_message_id().await?;
    Ok(())
}

#[cfg(test)]
mod live_tests {
    use super::setup_keyboard;

    /// Live registration check for the KWin `KeyboardMonitor` grab. Auto-skips unless a
    /// session bus *and* the a11y `KeyboardMonitor` provider are present, so it is a no-op in
    /// CI/headless and only exercises `SetKeyGrabs` on a KWin/Wayland desktop. It tears the
    /// grab down immediately so it doesn't disturb an interactive session.
    #[tokio::test]
    async fn keyboard_grab_registers_when_compositor_present() {
        let Ok(session) = zbus::Connection::session().await else {
            eprintln!("skipped: no session bus");
            return;
        };
        let Ok(dbus) = zbus::fdo::DBusProxy::new(&session).await else {
            eprintln!("skipped: no DBus proxy");
            return;
        };
        let name = match zbus::names::BusName::try_from("org.freedesktop.a11y.Manager") {
            Ok(name) => name,
            Err(_) => return,
        };
        if !dbus.name_has_owner(name).await.unwrap_or(false) {
            eprintln!("skipped: no KWin KeyboardMonitor");
            return;
        }

        let keyboard = setup_keyboard().await;
        assert!(
            keyboard.is_some(),
            "setup_keyboard (claim name + watch + SetKeyGrabs) should succeed on a KWin session"
        );

        // Tear the grab/role down again so the test leaves no lasting state.
        if let Some(kb) = keyboard {
            let _ = kb.proxy.unwatch_keyboard().await;
            let _ = kb
                .session
                .release_name("org.gnome.Orca.KeyboardMonitor")
                .await;
            let _ = kb.a11y_status.set_screen_reader_enabled(false).await;
        }
    }
}
