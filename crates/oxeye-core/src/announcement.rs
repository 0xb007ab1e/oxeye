//! Announcement composition: decides *what the screen reader actually says* for a focused
//! element, given the user's [`Verbosity`] preference and any matching exclusion [`Action`].
//!
//! This is the platform-agnostic **functional core** of announcement policy — pure and
//! deterministic, so it is unit-tested without any accessibility back-end. Platform crates
//! read an element from the accessibility tree, build an [`Element`], and call [`compose`].

use crate::exclusions::{Action, Context};
use crate::settings::Verbosity;

/// Selected accessibility states worth announcing. Each is independent; a back-end maps the
/// platform's state flags onto these. All-`false` (the [`Default`]) means "nothing special".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct States {
    /// The element has a checked/unchecked state (checkbox, toggle, radio, check menu item).
    pub checkable: bool,
    /// It is currently checked (only meaningful when `checkable`).
    pub checked: bool,
    /// The element can be expanded/collapsed (tree item, disclosure, combo box).
    pub expandable: bool,
    /// It is currently expanded (only meaningful when `expandable`).
    pub expanded: bool,
    /// It is currently selected (list/grid item, tab).
    pub selected: bool,
    /// It is present but not currently actionable ("dimmed"/greyed out).
    pub disabled: bool,
    /// Filling it in is required (form field).
    pub required: bool,
    /// Activating it opens a popup/menu.
    pub has_popup: bool,
}

/// A described UI element read from the accessibility tree — the input to [`compose`].
#[derive(Clone, Copy, Debug)]
pub struct Element<'a> {
    /// Matchable identity (name, role, owning app) — also used for exclusion matching.
    pub ident: Context<'a>,
    /// Accessible description / help text (often empty); spoken only at [`Verbosity::High`].
    pub description: &'a str,
    /// Textual value for value-bearing widgets (slider, spin button, progress, entry), if any.
    /// Back-ends that do not read the platform value interface leave this `None`.
    pub value: Option<&'a str>,
    /// Notable states (checked, expanded, selected, …).
    pub states: States,
}

/// What to speak for an element, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announcement {
    /// The text to speak.
    pub text: String,
    /// Whether to interrupt in-progress speech. `false` for a de-prioritised
    /// ([`Action::LowerPriority`]) announcement, so it does not cut off current speech.
    pub interrupt: bool,
}

/// Maximum length of a summarised name before it is truncated (for [`Action::Summarize`]).
const SUMMARY_MAX_CHARS: usize = 40;

/// Compose the announcement for `element` under `verbosity`, honoring an optional exclusion
/// `action`.
///
/// Returns `None` when the element must not be announced at all ([`Action::Suppress`]).
/// [`Action::Summarize`] forces the shortened form regardless of verbosity;
/// [`Action::LowerPriority`] keeps the verbosity-appropriate text but marks it non-interrupting.
#[must_use]
pub fn compose(
    element: &Element<'_>,
    verbosity: Verbosity,
    action: Option<Action>,
) -> Option<Announcement> {
    let text = match action {
        Some(Action::Suppress) => return None,
        Some(Action::Summarize) => summary(element),
        _ => describe(element, verbosity),
    };
    let interrupt = action != Some(Action::LowerPriority);
    Some(Announcement { text, interrupt })
}

/// Build the full announcement: label, then (by verbosity) role, states, value, description,
/// and owning application, joined as a comma-separated phrase.
fn describe(element: &Element<'_>, verbosity: Verbosity) -> String {
    let id = &element.ident;
    let mut parts: Vec<String> = Vec::new();

    // Label: the name, or the role when unnamed.
    if id.name.is_empty() {
        parts.push(id.role.to_owned());
    } else {
        parts.push(id.name.to_owned());
        // The role is chrome at Low; spoken from Medium up (and never duplicated as the label).
        if verbosity >= Verbosity::Medium {
            parts.push(id.role.to_owned());
        }
    }

    // States and value carry meaning, so they are announced at every verbosity level.
    parts.extend(state_words(element.states));
    if let Some(value) = element.value {
        if !value.is_empty() {
            parts.push(value.to_owned());
        }
    }

    // Description and owning application are extra detail, for High only.
    if verbosity >= Verbosity::High {
        if !element.description.is_empty() {
            parts.push(element.description.to_owned());
        }
        if !id.app.is_empty() {
            parts.push(id.app.to_owned());
        }
    }

    parts.join(", ")
}

/// The spoken words for an element's notable states, in a stable order.
fn state_words(states: States) -> Vec<String> {
    let mut words = Vec::new();
    if states.checkable {
        let word = if states.checked {
            "checked"
        } else {
            "not checked"
        };
        words.push(word.to_owned());
    }
    if states.expandable {
        let word = if states.expanded {
            "expanded"
        } else {
            "collapsed"
        };
        words.push(word.to_owned());
    }
    if states.selected {
        words.push("selected".to_owned());
    }
    if states.disabled {
        words.push("dimmed".to_owned());
    }
    if states.required {
        words.push("required".to_owned());
    }
    if states.has_popup {
        words.push("has popup".to_owned());
    }
    words
}

/// A shortened announcement: the first line of the name, length-capped, plus the role.
fn summary(element: &Element<'_>) -> String {
    let name = element.ident.name;
    let role = element.ident.role;
    let first_line = name.lines().next().unwrap_or(name).trim();
    let mut short: String = first_line.chars().take(SUMMARY_MAX_CHARS).collect();
    if first_line.chars().count() > SUMMARY_MAX_CHARS {
        short.push('…');
    }
    if short.is_empty() {
        role.to_owned()
    } else {
        format!("{short}, {role}")
    }
}

#[cfg(test)]
mod tests {
    use super::{compose, Announcement, Element, States};
    use crate::exclusions::{Action, Context};
    use crate::settings::Verbosity;

    fn el<'a>(name: &'a str, role: &'a str, app: &'a str) -> Element<'a> {
        Element {
            ident: Context { name, role, app },
            description: "",
            value: None,
            states: States::default(),
        }
    }

    fn text(element: &Element<'_>, verbosity: Verbosity) -> String {
        compose(element, verbosity, None).unwrap().text
    }

    #[test]
    fn verbosity_controls_detail() {
        let e = el("OK", "push button", "installer");
        assert_eq!(text(&e, Verbosity::Low), "OK");
        assert_eq!(text(&e, Verbosity::Medium), "OK, push button");
        assert_eq!(text(&e, Verbosity::High), "OK, push button, installer");
    }

    #[test]
    fn unnamed_element_falls_back_to_role_at_every_level() {
        let e = el("", "panel", "installer");
        assert_eq!(text(&e, Verbosity::Low), "panel");
        assert_eq!(text(&e, Verbosity::Medium), "panel");
        // High still appends the app even when unnamed.
        assert_eq!(text(&e, Verbosity::High), "panel, installer");
    }

    #[test]
    fn checkable_state_is_spoken_even_at_low() {
        let mut e = el("Wi-Fi", "check box", "settings");
        e.states = States {
            checkable: true,
            checked: true,
            ..States::default()
        };
        assert_eq!(text(&e, Verbosity::Medium), "Wi-Fi, check box, checked");
        e.states.checked = false;
        assert_eq!(text(&e, Verbosity::Low), "Wi-Fi, not checked");
    }

    #[test]
    fn tree_item_reports_expansion_and_selection() {
        let mut e = el("Documents", "tree item", "files");
        e.states = States {
            expandable: true,
            expanded: false,
            selected: true,
            ..States::default()
        };
        assert_eq!(
            text(&e, Verbosity::Medium),
            "Documents, tree item, collapsed, selected"
        );
    }

    #[test]
    fn disabled_and_required_states() {
        let mut e = el("Submit", "push button", "form");
        e.states = States {
            disabled: true,
            ..States::default()
        };
        assert_eq!(text(&e, Verbosity::Low), "Submit, dimmed");
        e.states = States {
            required: true,
            ..States::default()
        };
        assert_eq!(text(&e, Verbosity::Medium), "Submit, push button, required");
    }

    #[test]
    fn value_is_announced_when_present() {
        let mut e = el("Volume", "slider", "mixer");
        e.value = Some("70%");
        assert_eq!(text(&e, Verbosity::Medium), "Volume, slider, 70%");
    }

    #[test]
    fn description_is_high_verbosity_only() {
        let mut e = el("Email", "entry", "mail");
        e.description = "Enter your work email";
        assert_eq!(text(&e, Verbosity::Medium), "Email, entry");
        assert_eq!(
            text(&e, Verbosity::High),
            "Email, entry, Enter your work email, mail"
        );
    }

    #[test]
    fn suppress_yields_nothing() {
        let e = el("secret", "label", "bank");
        assert_eq!(compose(&e, Verbosity::High, Some(Action::Suppress)), None);
    }

    #[test]
    fn summarize_overrides_verbosity_and_truncates() {
        let long = "x".repeat(100);
        let e = el(&long, "banner", "web");
        let ann = compose(&e, Verbosity::High, Some(Action::Summarize)).unwrap();
        assert!(ann.text.ends_with(", banner"));
        assert!(ann.text.contains('…'));
        assert!(ann.interrupt);
    }

    #[test]
    fn lower_priority_keeps_text_but_does_not_interrupt() {
        let e = el("Loading", "statusbar", "ide");
        let ann = compose(&e, Verbosity::Medium, Some(Action::LowerPriority)).unwrap();
        assert_eq!(
            ann,
            Announcement {
                text: "Loading, statusbar".to_owned(),
                interrupt: false,
            }
        );
    }
}
