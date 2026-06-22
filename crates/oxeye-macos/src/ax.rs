//! The macOS **Accessibility (AXAPI)** adapter.
//!
//! Reads the focused element via the system-wide `AXUIElement` and hands it to [`oxeye_core`]
//! for announcement composition — the same policy that drives the Linux and Windows back-ends.
//! v1 **polls** the focused element and prints announcements (`[say] …`); AX notifications
//! (`AXObserver`), speech (`AVSpeechSynthesizer`/`say`), states, value, and structured
//! navigation are follow-ups.
//!
//! AXAPI is a C/FFI boundary; `unsafe` is confined to this module, and each block carries a
//! `// SAFETY:` justification (enforced by clippy's `undocumented_unsafe_blocks`).

use std::time::Duration;

use accessibility_sys::{
    kAXErrorSuccess, kAXFocusedUIElementAttribute, kAXRoleAttribute, kAXTitleAttribute,
    AXIsProcessTrusted, AXUIElementCopyAttributeValue, AXUIElementCreateSystemWide, AXUIElementRef,
};
use anyhow::{bail, Result};
use core_foundation::base::{CFType, TCFType};
use core_foundation::string::CFString;
use core_foundation_sys::base::CFTypeRef;
use oxeye_core::announcement::{self, Element, States};
use oxeye_core::exclusions::{Context as AxContext, ExclusionEngine};
use oxeye_core::{Settings, Verbosity};

/// How often the focused element is polled (AX notifications are a follow-up).
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Read focus in a loop and announce changes as text.
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

    eprintln!("oxeye-macos: reading focus (text output). Ctrl-C to quit.");
    let mut last = String::new();
    loop {
        if let Some(text) = read_focused(system, &exclusions, settings.verbosity) {
            if text != last {
                println!("[say] {text}");
                last = text;
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Read the focused element's role and title and compose its announcement via the shared core
/// policy. Returns `None` when there is no focus or an exclusion suppresses it.
fn read_focused(
    system: AXUIElementRef,
    exclusions: &ExclusionEngine,
    verbosity: Verbosity,
) -> Option<String> {
    let focused = copy_attribute(system, kAXFocusedUIElementAttribute)?;
    let focused_ref = focused.as_concrete_TypeRef() as AXUIElementRef;
    let role_id = copy_string(focused_ref, kAXRoleAttribute).unwrap_or_default();
    let title = copy_string(focused_ref, kAXTitleAttribute).unwrap_or_default();

    let ident = AxContext {
        app: "",
        role: ax_role_label(&role_id),
        name: &title,
    };
    let action = exclusions.evaluate(&ident);
    let element = Element {
        ident,
        description: "",
        // Value (AXValue) and states (AXEnabled, AXValue for toggles, …) are follow-ups.
        value: None,
        states: States::default(),
    };
    announcement::compose(&element, verbosity, action).map(|ann| ann.text)
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
