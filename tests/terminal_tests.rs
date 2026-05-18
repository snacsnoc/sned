//! Terminal feature integration tests.
//!
//! These tests verify the terminal feature works end-to-end when
//! the `terminal` feature is enabled.

// ghostty_backend.rs was removed in Phase 33.2 - vt_renderer.rs now provides VT parsing.
// terminal feature is now empty (libghostty-vt removed), so these tests are disabled.

#[cfg(feature = "terminal")]
mod terminal_tests {
    // Tests for ghostty_backend and TerminalSurface were removed.
    // The vt_renderer module has its own inline tests.
}

#[cfg(not(feature = "terminal"))]
mod terminal_disabled {
    #[test]
    fn test_terminal_feature_disabled() {
        // Test placeholder for when terminal feature is disabled
    }
}
