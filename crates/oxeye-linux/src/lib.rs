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

use anyhow::{Context as _, Result};
use atspi::connection::AccessibilityConnection;
use atspi::events::object::StateChangedEvent;
use atspi::events::EventProperties;
use atspi::proxy::accessible::AccessibleProxy;
use atspi::{Event, ObjectEvents, State, StateSet};
use futures_lite::stream::StreamExt;
use ssip_client_async::fifo::asynchronous_tokio::Builder as SsipBuilder;
use ssip_client_async::tokio::AsyncClient;
use ssip_client_async::{ClientError, ClientName, ClientScope, MessageScope};
use tokio::io::{AsyncBufRead, AsyncWrite};

use oxeye_core::announcement;
use oxeye_core::exclusions::{Context as ExclusionContext, ExclusionEngine};
use oxeye_core::{Settings, Speech};

/// X keysyms for the keys we react to.
const KEYSYM_CONTROL_L: u32 = 0xffe3;
const KEYSYM_CONTROL_R: u32 = 0xffe4;
const KEYSYM_ALT_L: u32 = 0xffe9;
const KEYSYM_ALT_R: u32 = 0xffea;
const KEYSYM_PAUSE: u32 = 0xff13;
const KEYSYM_O: u32 = 0x6f;

/// X11 modifier-mask bits as reported in the `KeyEvent` `state` field.
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

/// Output sink for announcements: text, speech, or both.
struct Speaker {
    mode: SpeechMode,
    client: Option<SsipClient>,
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
        if let Some(client) = self.client.as_mut() {
            if interrupt {
                let _ = client.cancel(MessageScope::All).await;
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
            let _ = client.cancel(MessageScope::All).await;
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
    // Also grab a dedicated, *consumed* shortcut: Ctrl+Alt+O (won't reach the focused app).
    let modifiers = vec![
        KEYSYM_CONTROL_L,
        KEYSYM_CONTROL_R,
        KEYSYM_ALT_L,
        KEYSYM_ALT_R,
    ];
    let grabs = vec![(KEYSYM_O, MOD_CONTROL | MOD_ALT)];
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
    let mut speaker = Speaker { mode, client };

    // Accessibility: subscribe to focus changes.
    let conn = AccessibilityConnection::new()
        .await
        .context("connecting to the AT-SPI accessibility bus")?;
    conn.register_event::<StateChangedEvent>()
        .await
        .context("registering for state-changed events")?;
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

    loop {
        tokio::select! {
            Some(event) = atspi_events.next() => {
                let Ok(event) = event else { continue };
                let Event::Object(ObjectEvents::StateChanged(state)) = event else {
                    continue;
                };
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
                let ctx = ExclusionContext {
                    app: &focused.app,
                    role: &focused.role,
                    name: &focused.name,
                };
                let action = exclusions.evaluate(&ctx);
                let element = announcement::Element {
                    ident: ctx,
                    description: &focused.description,
                    // Numeric/text value via the AT-SPI Value/Text interface is a follow-up.
                    value: None,
                    states: focused.states,
                };
                let Some(ann) = announcement::compose(&element, settings.verbosity, action) else {
                    continue; // suppressed by an exclusion rule
                };
                last_text = Some(ann.text.clone());
                speaker.announce(&ann.text, ann.interrupt).await;
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
    apply_speech_settings(&mut tts, &settings.speech).await;
    Ok(tts)
}

/// Apply rate/pitch/volume/voice/language/output-module from settings (best-effort).
async fn apply_speech_settings(tts: &mut SsipClient, speech: &Speech) {
    if let Some(module) = &speech.output_module {
        let _ = tts.set_output_module(ClientScope::Current, module).await;
    }
    if let Some(voice) = &speech.voice {
        let _ = tts.set_synthesis_voice(ClientScope::Current, voice).await;
    }
    if let Some(lang) = &speech.language {
        let _ = tts.set_language(ClientScope::Current, lang).await;
    }
    let _ = tts
        .set_rate(ClientScope::Current, to_ssip_scale(speech.rate))
        .await;
    let _ = tts
        .set_pitch(ClientScope::Current, to_ssip_scale(speech.pitch))
        .await;
    let _ = tts
        .set_volume(ClientScope::Current, to_ssip_scale(speech.volume))
        .await;
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
    states: announcement::States,
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
    let states = match proxy.get_state().await {
        Ok(set) => states_from(set),
        Err(err) => {
            tracing::debug!(%err, %sender, %path, "get_state() failed");
            announcement::States::default()
        }
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
        states,
    })
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
    tts.speak().await?.send_line(text).await?;
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
