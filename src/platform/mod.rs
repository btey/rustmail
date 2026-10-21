//! Host integration layer. Shared code selects behaviour with the `desktop`
//! and `linux_desktop` aliases from build.rs and calls the items here.
//! See platform/README.md.

/// Build the platform context for the process launcher in src/desktop.rs.
pub fn launch_context() -> Result<crate::PlatformContext, rustmail_core::error::CoreError> {
    crate::PlatformContext::desktop()
}
