//! The user-defined exclusions engine: rules that suppress, summarise, or de-prioritise
//! announcements for noisy apps, regions, or controls.
//!
//! Rules are plain data (serialised into the user's config) and are compiled into an
//! [`ExclusionEngine`] for evaluation. Name matching uses the `regex` crate, whose
//! linear-time engine is not vulnerable to ReDoS even on user-supplied patterns.

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// What to do when a rule matches an announcement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// Do not announce at all.
    Suppress,
    /// Announce a shortened summary instead of the full content.
    Summarize,
    /// Announce, but at a lower priority than normal.
    LowerPriority,
}

/// A single, serialisable exclusion rule.
///
/// A field set to `None` matches anything. A rule matches when *all* of its set fields
/// match the [`Context`] of the announcement.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExclusionRule {
    /// Restrict to a specific application (by accessible application name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Restrict to a specific accessibility role (e.g. `"statusbar"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Restrict to accessible names matching this regular expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_regex: Option<String>,
    /// Action to take when this rule matches.
    pub action: Action,
}

/// The attributes of a single announcement, evaluated against the rules.
#[derive(Clone, Copy, Debug)]
pub struct Context<'a> {
    /// Accessible application name the announcement originates from.
    pub app: &'a str,
    /// Accessibility role of the element.
    pub role: &'a str,
    /// Accessible name/text of the element.
    pub name: &'a str,
}

/// A compiled, ready-to-evaluate set of exclusion rules.
#[derive(Debug, Default)]
pub struct ExclusionEngine {
    rules: Vec<CompiledRule>,
}

#[derive(Debug)]
struct CompiledRule {
    app: Option<String>,
    role: Option<String>,
    name: Option<Regex>,
    action: Action,
}

impl ExclusionEngine {
    /// Compile a set of rules, validating any regular expressions.
    ///
    /// # Errors
    /// Returns [`crate::Error::InvalidRegex`] if a rule's `name_regex` is invalid; the
    /// engine fails closed rather than silently ignoring a malformed rule.
    pub fn compile(rules: &[ExclusionRule]) -> Result<Self> {
        let mut compiled = Vec::with_capacity(rules.len());
        for rule in rules {
            let name = match &rule.name_regex {
                Some(pattern) => Some(Regex::new(pattern)?),
                None => None,
            };
            compiled.push(CompiledRule {
                app: rule.app.clone(),
                role: rule.role.clone(),
                name,
                action: rule.action,
            });
        }
        Ok(Self { rules: compiled })
    }

    /// Evaluate an announcement, returning the action of the first matching rule, if any.
    #[must_use]
    pub fn evaluate(&self, ctx: &Context<'_>) -> Option<Action> {
        self.rules
            .iter()
            .find(|rule| rule.matches(ctx))
            .map(|rule| rule.action)
    }
}

impl CompiledRule {
    fn matches(&self, ctx: &Context<'_>) -> bool {
        self.app.as_deref().is_none_or(|a| a == ctx.app)
            && self.role.as_deref().is_none_or(|r| r == ctx.role)
            && self.name.as_ref().is_none_or(|re| re.is_match(ctx.name))
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, Context, ExclusionEngine, ExclusionRule};

    fn rule(
        app: Option<&str>,
        role: Option<&str>,
        name: Option<&str>,
        action: Action,
    ) -> ExclusionRule {
        ExclusionRule {
            app: app.map(str::to_owned),
            role: role.map(str::to_owned),
            name_regex: name.map(str::to_owned),
            action,
        }
    }

    #[test]
    fn suppresses_matching_app() {
        let engine =
            ExclusionEngine::compile(&[rule(Some("noisyapp"), None, None, Action::Suppress)])
                .unwrap();
        assert_eq!(
            engine.evaluate(&Context {
                app: "noisyapp",
                role: "statusbar",
                name: "x"
            }),
            Some(Action::Suppress)
        );
        assert_eq!(
            engine.evaluate(&Context {
                app: "editor",
                role: "statusbar",
                name: "x"
            }),
            None
        );
    }

    #[test]
    fn matches_name_by_regex() {
        let engine =
            ExclusionEngine::compile(&[rule(None, None, Some("(?i)cookie"), Action::Summarize)])
                .unwrap();
        assert_eq!(
            engine.evaluate(&Context {
                app: "web",
                role: "banner",
                name: "Cookie consent"
            }),
            Some(Action::Summarize)
        );
        assert_eq!(
            engine.evaluate(&Context {
                app: "web",
                role: "banner",
                name: "Article"
            }),
            None
        );
    }

    #[test]
    fn invalid_regex_fails_closed() {
        assert!(
            ExclusionEngine::compile(&[rule(None, None, Some("(unclosed"), Action::Suppress)])
                .is_err()
        );
    }

    #[test]
    fn first_matching_rule_wins() {
        let engine = ExclusionEngine::compile(&[
            rule(Some("app"), None, None, Action::LowerPriority),
            rule(Some("app"), None, None, Action::Suppress),
        ])
        .unwrap();
        assert_eq!(
            engine.evaluate(&Context {
                app: "app",
                role: "x",
                name: "y"
            }),
            Some(Action::LowerPriority)
        );
    }
}
