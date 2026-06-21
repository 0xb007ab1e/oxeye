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
use atspi::{Event, ObjectEvents, State};
use futures_lite::stream::StreamExt;
use ssip_client_async::fifo::asynchronous_tokio::Builder as SsipBuilder;
use ssip_client_async::tokio::AsyncClient;
use ssip_client_async::{ClientError, ClientName, ClientScope, MessageScope};
use tokio::io::{AsyncBufRead, AsyncWrite};

use oxeye_core::exclusions::{Context as ExclusionContext, ExclusionEngine};
use oxeye_core::{Action, Settings};

/// X keysyms for the keys we react to.
const KEYSYM_CONTROL_L: u32 = 0xffe3;
const KEYSYM_CONTROL_R: u32 = 0xffe4;
const KEYSYM_PAUSE: u32 = 0xff13;

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
    /// Announce `text`, interrupting any in-progress speech.
    async fn announce(&mut self, text: &str) {
        if self.mode.wants_text() {
            println!("[say] {text}");
        }
        if let Some(client) = self.client.as_mut() {
            let _ = client.cancel(MessageScope::All).await;
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

#[tokio::main]
async fn main() -> Result<()> {
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

    // Hotkeys: KWin's a11y KeyboardMonitor on the session bus.
    let session = zbus::Connection::session()
        .await
        .context("connecting to the session bus")?;
    let a11y_status = A11yStatusProxy::new(&session)
        .await
        .context("connecting to org.a11y.Status")?;
    let _ = a11y_status.set_is_enabled(true).await;
    let _ = a11y_status.set_screen_reader_enabled(true).await;
    // KWin 6.3.x authorises KeyboardMonitor *only* for the owner of Orca's well-known name
    // (hardcoded in `a11ykeyboardmonitor.cpp`), so claim it on this connection first.
    // (TODO: drop once KWin generalises the check; release the name + reset flags on exit.)
    session
        .request_name("org.gnome.Orca.KeyboardMonitor")
        .await
        .context("claiming the screen-reader D-Bus name KWin requires for KeyboardMonitor")?;
    let keyboard = KeyboardMonitorProxy::new(&session)
        .await
        .context("connecting to KWin's a11y KeyboardMonitor")?;
    keyboard
        .watch_keyboard()
        .await
        .context("starting keyboard watch (is this a KWin/Wayland session?)")?;
    let key_events = keyboard.receive_key_event().await?;
    futures_lite::pin!(key_events);

    eprintln!(
        "oxeye spike ({}): Tab/Alt-Tab to hear focus · Control silences · Pause repeats · Ctrl-C quits.",
        match mode {
            SpeechMode::Speech => "speech",
            SpeechMode::Text => "text",
            SpeechMode::Both => "speech+text",
        }
    );
    speaker.announce("oxeye spike running").await;

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
                let (app, name, role) = match read_focus(&conn, &state).await {
                    Ok(triple) => triple,
                    Err(err) => {
                        tracing::debug!(%err, "could not describe focused element");
                        continue;
                    }
                };
                let ctx = ExclusionContext { app: &app, role: &role, name: &name };
                if exclusions.evaluate(&ctx) == Some(Action::Suppress) {
                    continue;
                }
                let text = format!("{name}, {role}");
                last_text = Some(text.clone());
                speaker.announce(&text).await;
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
                        speaker.announce(&text).await;
                    }
                    _ => {}
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down: releasing screen-reader role");
                break;
            }
            else => break,
        }
    }

    // Graceful shutdown: stop watching keys, release the Orca name, and clear the a11y
    // flags so the desktop doesn't stay in "screen reader active" state after we exit.
    let _ = keyboard.unwatch_keyboard().await;
    let _ = session.release_name("org.gnome.Orca.KeyboardMonitor").await;
    let _ = a11y_status.set_screen_reader_enabled(false).await;
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
    let rate = ((i16::from(settings.speech_rate) - 50) * 2).clamp(-100, 100) as i8;
    let _ = tts.set_rate(ClientScope::Current, rate).await;
    Ok(tts)
}

/// Build an accessible proxy for the event's object and return `(app, name, role)`.
async fn read_focus(
    conn: &AccessibilityConnection,
    ev: &StateChangedEvent,
) -> Result<(String, String, String)> {
    let sender = ev.sender().to_string();
    let path = ev.path().to_string();
    let proxy = AccessibleProxy::builder(conn.connection())
        .destination(ev.sender())?
        .path(ev.path())?
        // Don't pre-fetch/cache properties (a GetAll on build): lighter, and avoids a
        // heavier code path on the app's a11y bridge.
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
    let app = read_app_name(conn, &proxy).await;
    if name.is_empty() {
        // Unnamed containers (panels/frames/fillers) are normal; not worth a warning.
        tracing::debug!(%sender, %path, %app, %role, "focused object has no accessible name");
    }
    Ok((app, name, role))
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
