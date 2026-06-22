//! The macOS **Accessibility (AXAPI)** adapter.
//!
//! Reads the focused element via the system-wide `AXUIElement` and hands it to [`oxeye_core`]
//! for announcement composition — the same policy that drives the Linux and Windows back-ends.
//! v1 **polls** the focused element and prints announcements (`[say] …`); AX notifications
//! (`AXObserver`), speech (`AVSpeechSynthesizer`/`say`), and structured navigation are
//! follow-ups. States (enabled/selected/expanded/checked) and the element's value are read here.
//!
//! AXAPI is a C/FFI boundary; `unsafe` is confined to this module, and each block carries a
//! `// SAFETY:` justification (enforced by clippy's `undocumented_unsafe_blocks`).

use std::time::Duration;

use accessibility_sys::{
    kAXCheckBoxRole, kAXEnabledAttribute, kAXErrorSuccess, kAXExpandedAttribute,
    kAXFocusedUIElementAttribute, kAXRadioButtonRole, kAXRoleAttribute, kAXSecureTextFieldSubrole,
    kAXSelectedAttribute, kAXSubroleAttribute, kAXTitleAttribute, kAXValueAttribute,
    AXIsProcessTrusted, AXUIElementCopyAttributeValue, AXUIElementCreateSystemWide, AXUIElementRef,
};
use anyhow::{bail, Result};
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::base::CFTypeRef;
use oxeye_core::announcement::{self, Element, States};
use oxeye_core::exclusions::{Context as AxContext, ExclusionEngine};
use oxeye_core::{Settings, Verbosity};

/// How often the focused element is polled (AX notifications are a follow-up).
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Upper bound on a textual value's length (characters) before truncation, to keep speech
/// short. Mirrors the Windows back-end's bound.
const VALUE_MAX_CHARS: usize = 120;

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
    let subrole = copy_string(focused_ref, kAXSubroleAttribute).unwrap_or_default();
    let title = copy_string(focused_ref, kAXTitleAttribute).unwrap_or_default();

    let ident = AxContext {
        app: "",
        role: ax_role_label(&role_id),
        name: &title,
    };
    let action = exclusions.evaluate(&ident);
    let states = read_states(focused_ref, &role_id);
    let value = read_value(focused_ref, &subrole, states.checkable);
    let element = Element {
        ident,
        description: "",
        value: value.as_deref(),
        states,
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
