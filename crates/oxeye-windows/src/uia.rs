//! The Windows **UI Automation** adapter + **SAPI** speech output.
//!
//! Registers an `IUIAutomationFocusChangedEventHandler` (a COM event sink) and hands each
//! focused element to [`oxeye_core`] for announcement composition — the same policy that drives
//! the Linux back-end. Output is SAPI speech (`ISpVoice`) and/or text, chosen by `OXEYE_SPEECH`
//! (`speech` default, `text`, or `both`).
//!
//! COM/UIA/SAPI are `unsafe` FFI boundaries, confined to this module. Two design points:
//! - **MTA**: UIA invokes the focus handler on its own worker threads, so no window message
//!   pump is needed; the main thread parks.
//! - COM interface pointers are `!Send`/`!Sync`, but the focus handler is shared across UIA
//!   threads. So the `ISpVoice` lives on a **dedicated speech thread**, fed over a `SyncSender`
//!   (which is `Send + Sync`); the handler never touches the voice directly.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::time::Duration;

use anyhow::{Context as _, Result};
use oxeye_core::announcement::{self, Announcement, Element, States};
use oxeye_core::exclusions::{Context as UiaContext, ExclusionEngine};
use oxeye_core::{Settings, Verbosity};
use windows::core::{implement, PCWSTR};
use windows::Win32::Media::Speech::{ISpVoice, SpVoice, SPF_ASYNC, SPF_PURGEBEFORESPEAK};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::core::Interface;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, ExpandCollapseState_Expanded, ExpandCollapseState_LeafNode, IUIAutomation,
    IUIAutomationCacheRequest, IUIAutomationElement, IUIAutomationExpandCollapsePattern,
    IUIAutomationFocusChangedEventHandler, IUIAutomationFocusChangedEventHandler_Impl,
    IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern, ToggleState_On,
    UIA_ButtonControlTypeId, UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId,
    UIA_EditControlTypeId, UIA_ExpandCollapsePatternId, UIA_HyperlinkControlTypeId,
    UIA_ListItemControlTypeId, UIA_MenuItemControlTypeId, UIA_RadioButtonControlTypeId,
    UIA_SelectionItemPatternId, UIA_TabItemControlTypeId, UIA_TextControlTypeId,
    UIA_TogglePatternId, UIA_CONTROLTYPE_ID, UIA_PATTERN_ID,
};

/// One announcement bound for the speech thread.
struct Utterance {
    text: String,
    /// Interrupt in-progress speech (purge the queue) vs. append.
    interrupt: bool,
}

/// How announcements are emitted, chosen by the `OXEYE_SPEECH` environment variable.
#[derive(Clone, Copy)]
enum SpeechMode {
    /// SAPI speech only (default).
    Speech,
    /// Print to stdout only (no audio) — for headless dev.
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

/// A UI Automation focus-changed event sink. Holds read-only, `Sync` state so it can announce
/// from UIA's worker threads; speech is delegated to a separate thread via `speech`.
#[implement(IUIAutomationFocusChangedEventHandler)]
struct FocusHandler {
    exclusions: ExclusionEngine,
    verbosity: Verbosity,
    print: bool,
    speech: Option<SyncSender<Utterance>>,
}

impl IUIAutomationFocusChangedEventHandler_Impl for FocusHandler_Impl {
    fn HandleFocusChangedEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
    ) -> windows::core::Result<()> {
        if let Some(element) = sender {
            if let Some(ann) = describe(element, &self.exclusions, self.verbosity) {
                if self.print {
                    println!("[say] {}", ann.text);
                }
                if let Some(tx) = &self.speech {
                    // Never block a UIA worker thread: drop if the speech queue is full.
                    let _ = tx.try_send(Utterance {
                        text: ann.text,
                        interrupt: ann.interrupt,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Initialize UI Automation, register the focus-changed handler, and keep it alive.
pub(crate) fn run() -> Result<()> {
    let settings = Settings::load().unwrap_or_default();
    let exclusions = ExclusionEngine::compile(&settings.exclusions).unwrap_or_default();
    let mode = SpeechMode::from_env();

    // SAFETY: standard per-thread COM apartment initialization (MTA); released at exit.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .context("CoInitializeEx")?;
    // SAFETY: create the UI Automation root object via COM.
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .context("creating the UI Automation client")?;

    let speech = mode.wants_audio().then(spawn_speech_thread);
    let handler: IUIAutomationFocusChangedEventHandler = FocusHandler {
        exclusions,
        verbosity: settings.verbosity,
        print: mode.wants_text(),
        speech,
    }
    .into();
    // SAFETY: register the sink; UIA invokes it on its own threads until exit.
    unsafe { automation.AddFocusChangedEventHandler(None::<&IUIAutomationCacheRequest>, &handler) }
        .context("AddFocusChangedEventHandler")?;

    eprintln!("oxeye-windows: listening for focus changes. Ctrl-C to quit.");
    // The handler fires on UIA worker threads; keep this thread and the registration alive.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Spawn the speech thread (it owns the `!Send` `ISpVoice`) and return a sender to it.
fn spawn_speech_thread() -> SyncSender<Utterance> {
    let (tx, rx) = sync_channel::<Utterance>(32);
    std::thread::spawn(move || speech_loop(&rx));
    tx
}

/// Own a SAPI voice and speak each received utterance. Exits silently if SAPI is unavailable.
fn speech_loop(rx: &Receiver<Utterance>) {
    // SAFETY: COM init for this thread (MTA); the voice is created and used only here.
    if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .is_err()
    {
        return;
    }
    // SAFETY: create the SAPI voice via COM.
    let voice: ISpVoice = match unsafe { CoCreateInstance(&SpVoice, None, CLSCTX_INPROC_SERVER) } {
        Ok(voice) => voice,
        Err(err) => {
            tracing::warn!(%err, "SAPI voice unavailable; speech disabled");
            return;
        }
    };
    while let Ok(utterance) = rx.recv() {
        let mut wide: Vec<u16> = utterance.text.encode_utf16().collect();
        wide.push(0); // null-terminate
        let mut flags = SPF_ASYNC.0 as u32;
        if utterance.interrupt {
            flags |= SPF_PURGEBEFORESPEAK.0 as u32;
        }
        // SAFETY: speak the null-terminated wide string; SAPI copies it before returning.
        if let Err(err) = unsafe { voice.Speak(PCWSTR(wide.as_ptr()), flags, None) } {
            tracing::debug!(%err, "SAPI Speak failed");
        }
    }
}

/// Read the focused element's name and control type and compose its announcement via the shared
/// core policy. Returns `None` when an exclusion suppresses it.
fn describe(
    element: &IUIAutomationElement,
    exclusions: &ExclusionEngine,
    verbosity: Verbosity,
) -> Option<Announcement> {
    // SAFETY: UIA COM calls reading the element's properties.
    let name = unsafe { element.CurrentName() }
        .map(|bstr| bstr.to_string())
        .unwrap_or_default();
    let control_type = unsafe { element.CurrentControlType() }.unwrap_or(UIA_CONTROLTYPE_ID(0));
    let role = control_type_role(control_type);

    let ident = UiaContext {
        app: "",
        role,
        name: &name,
    };
    let states = read_states(element);
    let action = exclusions.evaluate(&ident);
    let element = Element {
        ident,
        description: "",
        // Value (UIA Value/RangeValue patterns) is a follow-up.
        value: None,
        states,
    };
    announcement::compose(&element, verbosity, action)
}

/// Query a UIA control pattern, returning `None` when the element doesn't support it.
fn pattern<T: Interface>(element: &IUIAutomationElement, id: UIA_PATTERN_ID) -> Option<T> {
    // SAFETY: GetCurrentPatternAs yields Err (null) for an unsupported pattern.
    unsafe { element.GetCurrentPatternAs::<T>(id) }.ok()
}

/// Map UIA patterns/properties onto the core [`States`]: disabled (IsEnabled), checkable/checked
/// (Toggle), expandable/expanded (ExpandCollapse), selected (SelectionItem). `required` and
/// `has_popup` need VARIANT property reads and are follow-ups.
fn read_states(element: &IUIAutomationElement) -> States {
    let mut states = States::default();

    // SAFETY: read the element's enabled flag.
    if let Ok(enabled) = unsafe { element.CurrentIsEnabled() } {
        states.disabled = !enabled.as_bool();
    }
    if let Some(toggle) = pattern::<IUIAutomationTogglePattern>(element, UIA_TogglePatternId) {
        states.checkable = true;
        // SAFETY: read the toggle state of a checkable control.
        if let Ok(state) = unsafe { toggle.CurrentToggleState() } {
            states.checked = state == ToggleState_On;
        }
    }
    if let Some(ec) =
        pattern::<IUIAutomationExpandCollapsePattern>(element, UIA_ExpandCollapsePatternId)
    {
        // SAFETY: read the expand/collapse state.
        if let Ok(state) = unsafe { ec.CurrentExpandCollapseState() } {
            states.expandable = state != ExpandCollapseState_LeafNode;
            states.expanded = state == ExpandCollapseState_Expanded;
        }
    }
    if let Some(item) =
        pattern::<IUIAutomationSelectionItemPattern>(element, UIA_SelectionItemPatternId)
    {
        // SAFETY: read whether the item is selected.
        if let Ok(selected) = unsafe { item.CurrentIsSelected() } {
            states.selected = selected.as_bool();
        }
    }
    states
}

/// Map a UIA control type to a human-readable role label for announcements.
fn control_type_role(control_type: UIA_CONTROLTYPE_ID) -> &'static str {
    if control_type == UIA_ButtonControlTypeId {
        "button"
    } else if control_type == UIA_EditControlTypeId {
        "edit"
    } else if control_type == UIA_TextControlTypeId {
        "text"
    } else if control_type == UIA_HyperlinkControlTypeId {
        "link"
    } else if control_type == UIA_CheckBoxControlTypeId {
        "check box"
    } else if control_type == UIA_RadioButtonControlTypeId {
        "radio button"
    } else if control_type == UIA_ComboBoxControlTypeId {
        "combo box"
    } else if control_type == UIA_MenuItemControlTypeId {
        "menu item"
    } else if control_type == UIA_ListItemControlTypeId {
        "list item"
    } else if control_type == UIA_TabItemControlTypeId {
        "tab"
    } else {
        "element"
    }
}
