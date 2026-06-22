//! The macOS **Accessibility (AXAPI)** adapter.
//!
//! Reads the focused element via AXAPI and hands it to [`oxeye_core`] for announcement
//! composition — the same policy that drives the Linux and Windows back-ends, speaking via
//! AVFoundation (see [`speech`]). States (enabled/selected/expanded/checked) and the element's
//! value are read here.
//!
//! **Focus tracking is event-driven** via an [`AXObserver`](AXObserverCreate) on the focused
//! application's `kAXFocusedUIElementChangedNotification`, pumped on a `CFRunLoop`. macOS has no
//! system-wide focus event (the observer is per-application), so the observer is **re-targeted on
//! application switch**, detected by a low-frequency check of the focused-app pid between
//! bounded run-loop passes — event-driven within an app, a light poll only for app switches.
//! (`NSWorkspace`-based activation tracking, and structured navigation, are follow-ups.)
//!
//! [`speech`]: crate::speech
//!
//! AXAPI is a C/FFI boundary; `unsafe` is confined to this module, and each block carries a
//! `// SAFETY:` justification (enforced by clippy's `undocumented_unsafe_blocks`).

use std::ffi::c_void;
use std::time::Duration;

use accessibility_sys::{
    kAXCheckBoxRole, kAXEnabledAttribute, kAXErrorSuccess, kAXExpandedAttribute,
    kAXFocusedApplicationAttribute, kAXFocusedUIElementAttribute,
    kAXFocusedUIElementChangedNotification, kAXFocusedWindowChangedNotification,
    kAXRadioButtonRole, kAXRoleAttribute, kAXSecureTextFieldSubrole, kAXSelectedAttribute,
    kAXSubroleAttribute, kAXTitleAttribute, kAXValueAttribute, AXIsProcessTrusted,
    AXObserverAddNotification, AXObserverCreate, AXObserverGetRunLoopSource, AXObserverRef,
    AXObserverRemoveNotification, AXUIElementCopyAttributeValue, AXUIElementCreateSystemWide,
    AXUIElementGetPid, AXUIElementRef,
};
use anyhow::{bail, Result};
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFRelease, CFTypeRef};
use core_foundation_sys::runloop::{
    kCFRunLoopDefaultMode, CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRemoveSource,
    CFRunLoopRunInMode,
};
use core_foundation_sys::string::CFStringRef;
use oxeye_core::announcement::{self, Announcement, Element, States};
use oxeye_core::exclusions::{Context as AxContext, ExclusionEngine};
use oxeye_core::{Settings, Verbosity};

use crate::speech::Speaker;

/// Seconds the run loop pumps before we re-check the focused application (app-switch detection).
/// Within an application, focus changes arrive as observer callbacks during this window; this
/// interval only bounds how quickly an application *switch* is noticed.
const APP_SWITCH_POLL_SECS: f64 = 0.5;

/// Upper bound on a textual value's length (characters) before truncation, to keep speech
/// short. Mirrors the Windows back-end's bound.
const VALUE_MAX_CHARS: usize = 120;

/// Shared state the observer callback reaches through its `refcon` pointer. Owns everything it
/// needs (no borrows) so the raw-pointer round-trip is sound; created once in [`run`] and lives
/// for the run loop's lifetime. Used only on the run-loop (main) thread.
struct Observed {
    speaker: Speaker,
    exclusions: ExclusionEngine,
    verbosity: Verbosity,
    /// Last spoken text, for de-duping repeats (matches the other back-ends).
    last: String,
}

/// An `AXObserver` bound to one application, with its run-loop source installed.
struct AppObserver {
    observer: AXObserverRef,
    /// Keeps the focused-application `AXUIElement` alive (released on drop); also the element the
    /// notifications are deregistered from in [`teardown`](Self::teardown).
    app: CFType,
    pid: i32,
}

impl AppObserver {
    /// Deregister the notifications, remove the run-loop source, and release the observer. The
    /// `app` element is released when this value drops.
    fn teardown(self) {
        let app_ref = self.app.as_concrete_TypeRef() as AXUIElementRef;
        // SAFETY: deregister the notifications from the same element they were added on, remove
        // the observer's source from the current run loop, then release the observer (a Core
        // Foundation object freed with `CFRelease`).
        unsafe {
            for notification in [
                kAXFocusedUIElementChangedNotification,
                kAXFocusedWindowChangedNotification,
            ] {
                let key = CFString::new(notification);
                AXObserverRemoveNotification(self.observer, app_ref, key.as_concrete_TypeRef());
            }
            let source = AXObserverGetRunLoopSource(self.observer);
            CFRunLoopRemoveSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            CFRelease(self.observer as CFTypeRef);
        }
    }
}

/// Run the macOS back-end: observe focus changes and speak them.
///
/// Sets up an [`AXObserver`](AXObserverCreate) on the focused application, pumps the run loop in
/// bounded passes, and re-targets the observer when the focused application changes.
pub(crate) fn run() -> Result<()> {
    // SAFETY: query whether this process is a trusted accessibility client.
    if !unsafe { AXIsProcessTrusted() } {
        bail!(
            "oxeye-macos needs Accessibility permission \
             (System Settings → Privacy & Security → Accessibility)"
        );
    }
    let settings = Settings::load().unwrap_or_default();
    let exclusions = ExclusionEngine::compile(&settings.exclusions).unwrap_or_default();
    // SAFETY: create the system-wide accessibility element (owned for the process lifetime).
    let system = unsafe { AXUIElementCreateSystemWide() };

    let mut observed = Observed {
        speaker: Speaker::new(),
        exclusions,
        verbosity: settings.verbosity,
        last: String::new(),
    };
    // A stable pointer to `observed` for the C callback's `refcon`. `observed` is never accessed
    // by name again — only through this pointer — so the borrow stack stays consistent
    // (`addr_of_mut!` avoids forming an intermediate `&mut`). It lives for the rest of this
    // (never-returning) function, so the pointer never dangles.
    let observed_ptr = std::ptr::addr_of_mut!(observed);
    let refcon = observed_ptr.cast::<c_void>();

    eprintln!("oxeye-macos: announcing focus changes (AXObserver). Ctrl-C to quit.");
    let mut current = setup_observer(system, refcon);
    // The observer fires only on *changes*, so announce whatever is focused right now.
    announce_current_focus(system, observed_ptr);

    loop {
        if current.is_some() {
            // SAFETY: pump this thread's run loop in the default mode for a bounded interval;
            // observer callbacks fire (and speak) here.
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, APP_SWITCH_POLL_SECS, 0) };
        } else {
            // No app to observe (e.g. nothing focused yet): the run loop would return instantly
            // with no sources, so wait instead of spinning.
            std::thread::sleep(Duration::from_secs_f64(APP_SWITCH_POLL_SECS));
        }
        // Re-target the observer if the focused application changed.
        if focused_app_pid(system) != current.as_ref().map(|observer| observer.pid) {
            if let Some(old) = current.take() {
                old.teardown();
            }
            current = setup_observer(system, refcon);
            announce_current_focus(system, observed_ptr);
        }
    }
}

/// Create an `AXObserver` on the currently-focused application, register the focus/window
/// change notifications, and install its source on the current run loop. Returns `None` when
/// there is no focused application or the observer cannot be created.
fn setup_observer(system: AXUIElementRef, refcon: *mut c_void) -> Option<AppObserver> {
    let app = copy_attribute(system, kAXFocusedApplicationAttribute)?;
    let app_ref = app.as_concrete_TypeRef() as AXUIElementRef;
    let mut pid: i32 = 0;
    // SAFETY: read the focused application's process id into `pid`.
    if unsafe { AXUIElementGetPid(app_ref, &mut pid) } != kAXErrorSuccess || pid <= 0 {
        return None;
    }
    let mut observer: AXObserverRef = std::ptr::null_mut();
    // SAFETY: create an observer for `pid` with our callback; on success `observer` is a +1
    // reference we own (released in `AppObserver::teardown`).
    if unsafe { AXObserverCreate(pid, focus_changed, &mut observer) } != kAXErrorSuccess
        || observer.is_null()
    {
        return None;
    }
    for notification in [
        kAXFocusedUIElementChangedNotification,
        kAXFocusedWindowChangedNotification,
    ] {
        let key = CFString::new(notification);
        // SAFETY: register the notification on the app element; `refcon` is handed back to the
        // callback. AX retains the notification name, so dropping `key` afterwards is fine.
        unsafe { AXObserverAddNotification(observer, app_ref, key.as_concrete_TypeRef(), refcon) };
    }
    // SAFETY: add the observer's run-loop source to the current run loop so callbacks are
    // delivered while it is pumped.
    unsafe {
        let source = AXObserverGetRunLoopSource(observer);
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
    }
    Some(AppObserver { observer, app, pid })
}

/// The process id of the currently-focused application, or `None` if there is none.
fn focused_app_pid(system: AXUIElementRef) -> Option<i32> {
    let app = copy_attribute(system, kAXFocusedApplicationAttribute)?;
    let app_ref = app.as_concrete_TypeRef() as AXUIElementRef;
    let mut pid: i32 = 0;
    // SAFETY: read the focused application's process id into `pid`.
    if unsafe { AXUIElementGetPid(app_ref, &mut pid) } == kAXErrorSuccess && pid > 0 {
        Some(pid)
    } else {
        None
    }
}

/// Read whatever element is focused system-wide right now and announce it. Used for the initial
/// focus and after an application switch (the observer only fires on subsequent *changes*).
fn announce_current_focus(system: AXUIElementRef, observed: *mut Observed) {
    let Some(focused) = copy_attribute(system, kAXFocusedUIElementAttribute) else {
        return;
    };
    let element = focused.as_concrete_TypeRef() as AXUIElementRef;
    // SAFETY: `observed` points at the `Observed` in `run`, which outlives the run loop; we are
    // on the run-loop thread and hold no other reference to it here.
    let observed = unsafe { &mut *observed };
    if let Some(ann) = describe(element, &observed.exclusions, observed.verbosity) {
        emit(observed, ann);
    }
}

/// `AXObserver` callback: a focus or window change fired. `element` is the newly focused element
/// — a **borrowed** reference owned by the system, so it is read but never released. `refcon` is
/// the [`Observed`] pointer registered in [`setup_observer`].
unsafe extern "C" fn focus_changed(
    _observer: AXObserverRef,
    element: AXUIElementRef,
    _notification: CFStringRef,
    refcon: *mut c_void,
) {
    // SAFETY: `refcon` is the `*mut Observed` we registered; it outlives the run loop, and run
    // loop callbacks are delivered on the single run-loop thread, so this `&mut` is exclusive.
    let observed = unsafe { &mut *refcon.cast::<Observed>() };
    if let Some(ann) = describe(element, &observed.exclusions, observed.verbosity) {
        emit(observed, ann);
    }
}

/// Speak an announcement, de-duping consecutive repeats of the same text.
fn emit(observed: &mut Observed, ann: Announcement) {
    if ann.text != observed.last {
        tracing::debug!(text = %ann.text, interrupt = ann.interrupt, "say");
        observed.speaker.speak(&ann.text, ann.interrupt);
        observed.last = ann.text;
    }
}

/// Read a focused element's role, states, and value and compose its announcement via the shared
/// core policy. Returns `None` when an exclusion suppresses it.
fn describe(
    element: AXUIElementRef,
    exclusions: &ExclusionEngine,
    verbosity: Verbosity,
) -> Option<Announcement> {
    let role_id = copy_string(element, kAXRoleAttribute).unwrap_or_default();
    let subrole = copy_string(element, kAXSubroleAttribute).unwrap_or_default();
    let title = copy_string(element, kAXTitleAttribute).unwrap_or_default();

    let ident = AxContext {
        app: "",
        role: ax_role_label(&role_id),
        name: &title,
    };
    let action = exclusions.evaluate(&ident);
    let states = read_states(element, &role_id);
    let value = read_value(element, &subrole, states.checkable);
    let described = Element {
        ident,
        description: "",
        value: value.as_deref(),
        states,
    };
    announcement::compose(&described, verbosity, action)
}

/// Copy an AX attribute as an owned Core Foundation value, or `None` if absent.
fn copy_attribute(element: AXUIElementRef, attribute: &str) -> Option<CFType> {
    let key = CFString::new(attribute);
    let mut value: CFTypeRef = std::ptr::null();
    let err =
        // SAFETY: copy the named attribute; on success `value` is a +1 reference we own.
        unsafe { AXUIElementCopyAttributeValue(element, key.as_concrete_TypeRef(), &mut value) };
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    // SAFETY: wrap the returned +1 reference so it is released on drop (the "create" rule).
    Some(unsafe { CFType::wrap_under_create_rule(value) })
}

/// Copy a string-valued AX attribute as a Rust `String`.
fn copy_string(element: AXUIElementRef, attribute: &str) -> Option<String> {
    copy_attribute(element, attribute)?
        .downcast::<CFString>()
        .map(|string| string.to_string())
}

/// Copy a boolean-valued AX attribute (`CFBoolean`), or `None` if absent / not a boolean.
fn copy_bool(element: AXUIElementRef, attribute: &str) -> Option<bool> {
    copy_attribute(element, attribute)?
        .downcast::<CFBoolean>()
        .map(bool::from)
}

/// Copy a numeric AX attribute (`CFNumber`) as `f64`, or `None` if absent / not a number.
fn copy_number(element: AXUIElementRef, attribute: &str) -> Option<f64> {
    copy_attribute(element, attribute)?
        .downcast::<CFNumber>()
        .and_then(|number| number.to_f64())
}

/// Map AXAPI attributes onto the core [`States`]: disabled (`AXEnabled`), selected (`AXSelected`),
/// expandable/expanded (`AXExpanded`), and checkable/checked (checkbox & radio roles, whose
/// `AXValue` is `0` off / `1` on / `2` mixed). `required` and `has_popup` have no standard AX
/// attribute and are left unset (a follow-up, as `has_popup` is on the Windows back-end).
fn read_states(element: AXUIElementRef, role_id: &str) -> States {
    let mut states = States::default();

    if let Some(enabled) = copy_bool(element, kAXEnabledAttribute) {
        states.disabled = !enabled;
    }
    if let Some(selected) = copy_bool(element, kAXSelectedAttribute) {
        states.selected = selected;
    }
    // Presence of AXExpanded means the element can disclose; its boolean is the current state.
    if let Some(expanded) = copy_bool(element, kAXExpandedAttribute) {
        states.expandable = true;
        states.expanded = expanded;
    }
    // Checkbox / radio report their check state as the numeric AXValue (1.0 == "on").
    if role_id == kAXCheckBoxRole || role_id == kAXRadioButtonRole {
        states.checkable = true;
        states.checked = copy_number(element, kAXValueAttribute)
            .is_some_and(|value| (value - 1.0).abs() < f64::EPSILON);
    }
    states
}

/// Read a textual value for the element, mirroring the Windows back-end's policy: a numeric
/// widget (slider / progress / stepper) via its `AXValue` formatted by
/// [`announcement::format_value`], else
/// editable text via the string `AXValue` — but **never** a secure (password) field, and **not**
/// a checkbox / radio (whose value is the check state, already carried in [`States`]). Bounded to
/// [`VALUE_MAX_CHARS`]. Returns `None` when there is no announceable value.
fn read_value(element: AXUIElementRef, subrole: &str, checkable: bool) -> Option<String> {
    // The checkbox / radio "value" is its check state, conveyed via States — don't repeat it.
    if checkable {
        return None;
    }
    // Never reveal a secure text field's contents.
    if subrole == kAXSecureTextFieldSubrole {
        return None;
    }
    let value = copy_attribute(element, kAXValueAttribute)?;
    if let Some(cf_number) = value.downcast::<CFNumber>() {
        let number = cf_number.to_f64()?;
        if number.is_finite() {
            return Some(announcement::format_value(number));
        }
        return None;
    }
    let text = value.downcast::<CFString>()?.to_string();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(VALUE_MAX_CHARS).collect())
}

/// Map an AX role identifier (e.g. `"AXButton"`) to a human-readable role label.
fn ax_role_label(role: &str) -> &'static str {
    match role {
        "AXButton" => "button",
        "AXStaticText" => "text",
        "AXTextField" | "AXTextArea" => "edit",
        "AXCheckBox" => "check box",
        "AXRadioButton" => "radio button",
        "AXLink" => "link",
        "AXMenuItem" => "menu item",
        "AXPopUpButton" | "AXComboBox" => "combo box",
        "AXSlider" => "slider",
        "AXHeading" => "heading",
        _ => "element",
    }
}
