//! `oxeye-macos` — the macOS back-end of the **oxeye** screen reader.
//!
//! Adapts the macOS **Accessibility API** (AXAPI: `AXUIElement`) — reading the focused element
//! and its attributes — and hands the data to the reusable, platform-agnostic policy in
//! [`oxeye_core`] (announcement composition, exclusions, verbosity, navigation, braille). The
//! same core that drives `oxeye-linux` and `oxeye-windows` drives this; only the
//! accessibility-tree, event, and output adapters differ.
//!
//! AXAPI is a C/FFI boundary requiring `unsafe`, confined to the [`ax`] module (see this crate's
//! `unsafe_code = "allow"`); `oxeye-core` itself stays `unsafe`-free.

#[cfg(target_os = "macos")]
mod ax;

/// Run the macOS screen-reader back-end.
///
/// On macOS this drives the Accessibility API; on other hosts it returns an error so the
/// workspace still builds and the binary fails cleanly.
///
/// # Errors
/// Propagates accessibility-permission / AXAPI failures (macOS), or a "macOS only" error on
/// other platforms.
#[cfg(target_os = "macos")]
pub fn run() -> anyhow::Result<()> {
    ax::run()
}

/// Stub entry point on non-macOS hosts: the back-end requires the macOS Accessibility APIs.
///
/// # Errors
/// Always returns an error indicating the current platform is unsupported.
#[cfg(not(target_os = "macos"))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!(
        "oxeye-macos requires the macOS Accessibility APIs; this host is {}",
        std::env::consts::OS
    )
}
