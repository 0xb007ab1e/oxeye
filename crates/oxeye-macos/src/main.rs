//! `oxeye-macos` binary — the macOS (Accessibility / AXAPI) screen-reader back-end.

fn main() -> anyhow::Result<()> {
    oxeye_macos::run()
}
