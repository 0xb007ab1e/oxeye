//! The Windows **UI Automation** adapter + **SAPI** speech output.
//!
//! Registers an `IUIAutomationFocusChangedEventHandler` (a COM event sink) and hands each
//! focused element to [`intone_core`] for announcement composition — the same policy that drives
//! the Linux back-end. Output is SAPI speech (`ISpVoice`) and/or text, chosen by `INTONE_SPEECH`
//! (`speech` default, `text`, or `both`).
//!
//! COM/UIA/SAPI are `unsafe` FFI boundaries, confined to this module. Two design points:
//! - **MTA**: UIA invokes the focus handler on its own worker threads, so no window message
//!   pump is needed; the main thread parks.
//! - COM interface pointers are `!Send`/`!Sync`, but the focus handler is shared across UIA
//!   threads. So the `ISpVoice` lives on a **dedicated speech thread**, fed over a `SyncSender`
//!   (which is `Send + Sync`); the handler never touches the voice directly.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use intone_core::announcement::{self, Announcement, Element, States};
use intone_core::exclusions::{Context as UiaContext, ExclusionEngine};
use intone_core::navigation::{self, Direction, NavCategory};
use intone_core::{Settings, Verbosity};
use windows::core::Interface;
use windows::core::{implement, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Media::Speech::{ISpVoice, SpVoice, SPF_ASYNC, SPF_PURGEBEFORESPEAK};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, ExpandCollapseState_Expanded, ExpandCollapseState_LeafNode, HeadingLevel_None,
    IUIAutomation, IUIAutomationCacheRequest, IUIAutomationCondition, IUIAutomationElement,
    IUIAutomationElement8, IUIAutomationElementArray, IUIAutomationExpandCollapsePattern,
    IUIAutomationFocusChangedEventHandler, IUIAutomationFocusChangedEventHandler_Impl,
    IUIAutomationRangeValuePattern, IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern,
    IUIAutomationValuePattern, ToggleState_On, TreeScope_Subtree, UIA_ButtonControlTypeId,
    UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId, UIA_EditControlTypeId,
    UIA_ExpandCollapsePatternId, UIA_HyperlinkControlTypeId, UIA_ListItemControlTypeId,
    UIA_MenuItemControlTypeId, UIA_RadioButtonControlTypeId, UIA_RangeValuePatternId,
    UIA_SelectionItemPatternId, UIA_TabItemControlTypeId, UIA_TextControlTypeId,
    UIA_TogglePatternId, UIA_ValuePatternId, UIA_CONTROLTYPE_ID, UIA_PATTERN_ID,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetMessageW, MSG, WM_HOTKEY};

/// Virtual-key codes for the navigation hotkeys (A–Z map to ASCII uppercase).
const VK_H: u32 = 0x48;
const VK_B: u32 = 0x42;
const VK_L: u32 = 0x4C;
const VK_F: u32 = 0x46;

/// Upper bound on text value read for an announcement, to avoid dumping a whole document.
const VALUE_MAX_CHARS: usize = 200;

/// One announcement bound for the speech thread.
struct Utterance {
    text: String,
    /// Interrupt in-progress speech (purge the queue) vs. append.
    interrupt: bool,
}

/// How announcements are emitted, chosen by the `INTONE_SPEECH` environment variable.
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
        match std::env::var("INTONE_SPEECH").as_deref() {
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
    exclusions: Arc<ExclusionEngine>,
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
                emit(ann, self.print, self.speech.as_ref());
            }
        }
        Ok(())
    }
}

/// Emit an announcement to the configured channels: print and/or speak (never blocking).
fn emit(ann: Announcement, print: bool, speech: Option<&SyncSender<Utterance>>) {
    if print {
        println!("[say] {}", ann.text);
    }
    if let Some(tx) = speech {
        // Never block the caller (a UIA worker thread): drop if the speech queue is full.
        let _ = tx.try_send(Utterance {
            text: ann.text,
            interrupt: ann.interrupt,
        });
    }
}

/// Initialize UI Automation, register the focus-changed handler, and keep it alive.
pub(crate) fn run() -> Result<()> {
    let settings = Settings::load().unwrap_or_default();
    let exclusions = Arc::new(ExclusionEngine::compile(&settings.exclusions).unwrap_or_default());
    let mode = SpeechMode::from_env();

    // SAFETY: standard per-thread COM apartment initialization (MTA); released at exit.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .context("CoInitializeEx")?;
    let automation: IUIAutomation =
        // SAFETY: create the UI Automation root object via COM.
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .context("creating the UI Automation client")?;

    let speech = mode
        .wants_audio()
        .then(|| spawn_speech_thread(settings.speech.rate, settings.speech.volume));
    let handler: IUIAutomationFocusChangedEventHandler = FocusHandler {
        exclusions: Arc::clone(&exclusions),
        verbosity: settings.verbosity,
        print: mode.wants_text(),
        speech: speech.clone(),
    }
    .into();
    // SAFETY: register the focus sink; UIA invokes it on its own threads.
    unsafe { automation.AddFocusChangedEventHandler(None::<&IUIAutomationCacheRequest>, &handler) }
        .context("AddFocusChangedEventHandler")?;
    register_hotkeys().context("RegisterHotKey")?;

    eprintln!(
        "intone-windows: focus + Ctrl+Alt+{{H,B,L,F}} navigation (Shift = previous). Ctrl-C to quit."
    );
    // The focus sink fires on UIA worker threads; this thread pumps WM_HOTKEY for navigation.
    // The virtual cursor is thread-local here (no cross-thread COM sharing).
    let mut cursor: Option<IUIAutomationElement> = None;
    let mut msg = MSG::default();
    loop {
        // SAFETY: block for the next thread message (hotkeys post WM_HOTKEY to this queue).
        let result = unsafe { GetMessageW(&mut msg, HWND::default(), 0, 0) };
        if result.0 <= 0 {
            break; // 0 = WM_QUIT, -1 = error
        }
        if msg.message == WM_HOTKEY {
            if let Some((category, direction)) = hotkey_action(msg.wParam.0 as i32) {
                navigate(
                    &automation,
                    &mut cursor,
                    category,
                    direction,
                    &exclusions,
                    settings.verbosity,
                    mode.wants_text(),
                    speech.as_ref(),
                );
            }
        }
    }
    Ok(())
}

/// Register the by-type navigation hotkeys: Ctrl+Alt+{B,L,F}, plus Shift for the previous match.
fn register_hotkeys() -> windows::core::Result<()> {
    let next = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
    let prev = next | MOD_SHIFT;
    let bindings = [
        (1, VK_B, next),
        (2, VK_L, next),
        (3, VK_F, next),
        (4, VK_B, prev),
        (5, VK_L, prev),
        (6, VK_F, prev),
        (7, VK_H, next),
        (8, VK_H, prev),
    ];
    for (id, vk, modifiers) in bindings {
        // SAFETY: a null HWND associates the hotkey with this thread's message queue.
        unsafe { RegisterHotKey(HWND::default(), id, modifiers, vk) }?;
    }
    Ok(())
}

/// Map a hotkey id to the navigation category and direction it requests.
fn hotkey_action(id: i32) -> Option<(NavCategory, Direction)> {
    match id {
        1 => Some((NavCategory::Button, Direction::Next)),
        2 => Some((NavCategory::Link, Direction::Next)),
        3 => Some((NavCategory::FormField, Direction::Next)),
        4 => Some((NavCategory::Button, Direction::Previous)),
        5 => Some((NavCategory::Link, Direction::Previous)),
        6 => Some((NavCategory::FormField, Direction::Previous)),
        7 => Some((NavCategory::Heading, Direction::Next)),
        8 => Some((NavCategory::Heading, Direction::Previous)),
        _ => None,
    }
}

/// Move the virtual cursor to the next/previous element of `target` type in the foreground
/// window and announce it. The UIA tree (in document order from `FindAll`) is classified, and
/// the shared core [`navigation::find`] selects the match relative to the current cursor.
#[allow(clippy::too_many_arguments)]
fn navigate(
    automation: &IUIAutomation,
    cursor: &mut Option<IUIAutomationElement>,
    target: NavCategory,
    direction: Direction,
    exclusions: &ExclusionEngine,
    verbosity: Verbosity,
    print: bool,
    speech: Option<&SyncSender<Utterance>>,
) {
    match collect_elements(automation) {
        Ok(elements) => {
            let categories: Vec<Option<NavCategory>> =
                elements.iter().map(element_category).collect();
            let from = current_index(automation, &elements, cursor.as_ref());
            if let Some(index) = navigation::find(&categories, from, target, direction) {
                let element = elements[index].clone();
                if let Some(ann) = describe(&element, exclusions, verbosity) {
                    emit(ann, print, speech);
                }
                *cursor = Some(element);
            } else {
                let dir = match direction {
                    Direction::Next => "next",
                    Direction::Previous => "previous",
                };
                let text = format!("no {dir} {}", target.singular());
                emit(
                    Announcement {
                        text,
                        interrupt: true,
                    },
                    print,
                    speech,
                );
            }
        }
        Err(err) => tracing::debug!(%err, "navigation tree walk failed"),
    }
}

/// Collect the foreground window's descendants in document order.
fn collect_elements(
    automation: &IUIAutomation,
) -> windows::core::Result<Vec<IUIAutomationElement>> {
    // SAFETY: resolve the foreground window to a UIA element and enumerate its subtree; all
    // calls operate on handles/objects obtained immediately above and valid for this scope.
    unsafe {
        let window = automation.ElementFromHandle(GetForegroundWindow())?;
        let condition: IUIAutomationCondition = automation.CreateTrueCondition()?;
        let all: IUIAutomationElementArray = window.FindAll(TreeScope_Subtree, &condition)?;
        let count = all.Length()?;
        let mut elements = Vec::with_capacity(count.max(0) as usize);
        for i in 0..count {
            elements.push(all.GetElement(i)?);
        }
        Ok(elements)
    }
}

/// Find the document-order index of the current cursor (or the focused element) within `elements`.
fn current_index(
    automation: &IUIAutomation,
    elements: &[IUIAutomationElement],
    cursor: Option<&IUIAutomationElement>,
) -> Option<usize> {
    let start = match cursor {
        Some(element) => element.clone(),
        None => {
            // SAFETY: fall back to the current focus when there is no cursor yet.
            unsafe { automation.GetFocusedElement() }.ok()?
        }
    };
    elements.iter().position(|el| {
        // SAFETY: UIA element identity comparison.
        unsafe { automation.CompareElements(el, &start) }
            .map(|equal| equal.as_bool())
            .unwrap_or(false)
    })
}

/// Read an element's control type, defaulting to the unknown type on error.
fn current_control_type(element: &IUIAutomationElement) -> UIA_CONTROLTYPE_ID {
    // SAFETY: read the control-type property.
    unsafe { element.CurrentControlType() }.unwrap_or(UIA_CONTROLTYPE_ID(0))
}

/// Classify an element for structured navigation: a heading (any element with a heading level
/// set — web/document content), else by its control type.
fn element_category(element: &IUIAutomationElement) -> Option<NavCategory> {
    // CurrentHeadingLevel lives on IUIAutomationElement8 (Win10 1709+); older elements simply
    // aren't classified as headings.
    if let Ok(element8) = element.cast::<IUIAutomationElement8>() {
        // SAFETY: read the heading level; HeadingLevel_None means it is not a heading.
        if let Ok(level) = unsafe { element8.CurrentHeadingLevel() } {
            if level != HeadingLevel_None {
                return Some(NavCategory::Heading);
            }
        }
    }
    control_type_category(current_control_type(element))
}

/// Map a UIA control type to a navigation category (buttons, links, form controls).
fn control_type_category(control_type: UIA_CONTROLTYPE_ID) -> Option<NavCategory> {
    if control_type == UIA_ButtonControlTypeId {
        Some(NavCategory::Button)
    } else if control_type == UIA_HyperlinkControlTypeId {
        Some(NavCategory::Link)
    } else if control_type == UIA_EditControlTypeId
        || control_type == UIA_ComboBoxControlTypeId
        || control_type == UIA_CheckBoxControlTypeId
        || control_type == UIA_RadioButtonControlTypeId
    {
        Some(NavCategory::FormField)
    } else {
        None
    }
}

/// Spawn the speech thread (it owns the `!Send` `ISpVoice`) and return a sender to it. `rate`
/// and `volume` are the user's 0–100 settings, applied once on the voice.
fn spawn_speech_thread(rate: u8, volume: u8) -> SyncSender<Utterance> {
    let (tx, rx) = sync_channel::<Utterance>(32);
    std::thread::spawn(move || speech_loop(&rx, rate, volume));
    tx
}

/// Map a 0–100 rate (50 = normal) onto SAPI's `SetRate` scale (-10..=10).
fn rate_to_sapi(value: u8) -> i32 {
    ((i32::from(value) - 50) / 5).clamp(-10, 10)
}

/// Own a SAPI voice and speak each received utterance. Exits silently if SAPI is unavailable.
fn speech_loop(rx: &Receiver<Utterance>, rate: u8, volume: u8) {
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
    // Apply the configured prosody (best-effort). Selecting a voice by name additionally needs
    // SAPI token enumeration (`SPCAT_VOICES`) + real-Windows verification — a follow-up.
    // SAFETY: simple prosody setters on the SAPI voice; errors are non-fatal.
    unsafe {
        let _ = voice.SetRate(rate_to_sapi(rate));
        let _ = voice.SetVolume(u16::from(volume).min(100));
    }
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
    // SAFETY: read the accessible name property.
    let name = unsafe { element.CurrentName() }
        .map(|bstr| bstr.to_string())
        .unwrap_or_default();
    let role = control_type_role(current_control_type(element));

    let ident = UiaContext {
        app: "",
        role,
        name: &name,
    };
    let states = read_states(element);
    let value = read_value(element);
    let action = exclusions.evaluate(&ident);
    let element = Element {
        ident,
        description: "",
        value: value.as_deref(),
        states,
    };
    announcement::compose(&element, verbosity, action)
}

/// Read a textual value for the element: numeric widgets (slider/spin/progress) via the
/// RangeValue pattern, else editable content via the Value pattern — but **never** a password
/// field. Bounded to [`VALUE_MAX_CHARS`]. Returns `None` when there is no value.
fn read_value(element: &IUIAutomationElement) -> Option<String> {
    if let Some(range) = pattern::<IUIAutomationRangeValuePattern>(element, UIA_RangeValuePatternId)
    {
        // SAFETY: read the numeric value of a range control.
        if let Ok(number) = unsafe { range.CurrentValue() } {
            if number.is_finite() {
                return Some(announcement::format_value(number));
            }
        }
    }
    // SAFETY: never reveal a password field's content.
    let is_password = unsafe { element.CurrentIsPassword() }
        .map(|flag| flag.as_bool())
        .unwrap_or(false);
    if is_password {
        return None;
    }
    let value = pattern::<IUIAutomationValuePattern>(element, UIA_ValuePatternId)?;
    // SAFETY: read the control's textual value.
    let text = unsafe { value.CurrentValue() }.ok()?.to_string();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(VALUE_MAX_CHARS).collect())
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
    // SAFETY: read whether the field is required for form completion.
    if let Ok(required) = unsafe { element.CurrentIsRequiredForForm() } {
        states.required = required.as_bool();
    }
    // `has_popup` is left unset: UIA has no direct accessor (it lives in the AriaProperties
    // string, which would need parsing). A follow-up if it proves useful.
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
