//! The Windows **UI Automation** adapter.
//!
//! Registers an `IUIAutomationFocusChangedEventHandler` (a COM event sink) and hands each
//! focused element to [`oxeye_core`] for announcement composition — the same policy that drives
//! the Linux back-end. v1 prints announcements (`[say] …`); SAPI speech, richer states, and
//! structured navigation are follow-ups.
//!
//! COM/UIA is an `unsafe` FFI boundary; it is confined to this module. We initialize a
//! multi-threaded apartment (MTA): UIA invokes the handler on its own worker threads, so no
//! window message pump is needed — the main thread simply keeps the registration alive.

use std::time::Duration;

use anyhow::{Context as _, Result};
use oxeye_core::announcement::{self, Element, States};
use oxeye_core::exclusions::{Context as UiaContext, ExclusionEngine};
use oxeye_core::{Settings, Verbosity};
use windows::core::implement;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationElement,
    IUIAutomationFocusChangedEventHandler, IUIAutomationFocusChangedEventHandler_Impl,
    UIA_ButtonControlTypeId, UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId,
    UIA_EditControlTypeId, UIA_HyperlinkControlTypeId, UIA_ListItemControlTypeId,
    UIA_MenuItemControlTypeId, UIA_RadioButtonControlTypeId, UIA_TabItemControlTypeId,
    UIA_TextControlTypeId, UIA_CONTROLTYPE_ID,
};

/// A UI Automation focus-changed event sink. Holds the (read-only, `Sync`) policy state so it
/// can announce from UIA's worker threads.
#[implement(IUIAutomationFocusChangedEventHandler)]
struct FocusHandler {
    exclusions: ExclusionEngine,
    verbosity: Verbosity,
}

impl IUIAutomationFocusChangedEventHandler_Impl for FocusHandler_Impl {
    fn HandleFocusChangedEvent(
        &self,
        sender: Option<&IUIAutomationElement>,
    ) -> windows::core::Result<()> {
        if let Some(element) = sender {
            if let Some(text) = describe(element, &self.exclusions, self.verbosity) {
                println!("[say] {text}");
            }
        }
        Ok(())
    }
}

/// Initialize UI Automation, register the focus-changed handler, and keep it alive.
pub(crate) fn run() -> Result<()> {
    let settings = Settings::load().unwrap_or_default();
    let exclusions = ExclusionEngine::compile(&settings.exclusions).unwrap_or_default();

    // SAFETY: standard per-thread COM apartment initialization (MTA); released at exit.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .context("CoInitializeEx")?;
    // SAFETY: create the UI Automation root object via COM.
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .context("creating the UI Automation client")?;

    let handler: IUIAutomationFocusChangedEventHandler = FocusHandler {
        exclusions,
        verbosity: settings.verbosity,
    }
    .into();
    // SAFETY: register the sink; UIA invokes it on its own threads until removed/exit.
    unsafe { automation.AddFocusChangedEventHandler(None::<&IUIAutomationCacheRequest>, &handler) }
        .context("AddFocusChangedEventHandler")?;

    eprintln!("oxeye-windows: listening for focus changes (text output). Ctrl-C to quit.");
    // The handler fires on UIA worker threads; keep this thread and the registration alive.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Read the focused element's name and control type and compose its announcement via the shared
/// core policy. Returns `None` when an exclusion suppresses it.
fn describe(
    element: &IUIAutomationElement,
    exclusions: &ExclusionEngine,
    verbosity: Verbosity,
) -> Option<String> {
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
    let action = exclusions.evaluate(&ident);
    let element = Element {
        ident,
        description: "",
        // Value (UIA Value/RangeValue patterns) and states are follow-ups.
        value: None,
        states: States::default(),
    };
    announcement::compose(&element, verbosity, action).map(|announcement| announcement.text)
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
