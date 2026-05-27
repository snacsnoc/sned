//! sned CLI – AI coding assistant in your terminal.
//!
//! This is the native Rust port of Sned, preserving the core differentiators:
//! - Hash-anchored edits
//! - AST-native structural tools
//! - Anchored multi-file batching
//! - Curated high-bandwidth context

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    let exit_code = match sned::cli::run() {
        Ok(()) => sned::exit_codes::EXIT_SUCCESS,
        Err(e) => {
            let exit_code = categorize_error(&e);
            eprintln!("Error: {}", e);
            exit_code
        }
    };

    std::process::exit(exit_code);
}

/// Categorize an error into an exit code using typed error matching
fn categorize_error(err: &anyhow::Error) -> i32 {
    // Try to downcast to CliError for typed exit code classification
    if let Some(cli_err) = err.downcast_ref::<sned::error::CliError>() {
        return cli_err.exit_code();
    }

    sned::exit_codes::EXIT_ERROR
}

#[cfg(test)]
mod tests {
    use super::*;
    use sned::error::CliError;
    use sned::exit_codes::*;

    #[test]
    fn test_categorize_cli_error_config() {
        assert_eq!(
            categorize_error(&anyhow::anyhow!(CliError::config("missing api key"))),
            EXIT_CONFIG
        );
        assert_eq!(
            categorize_error(&anyhow::anyhow!(CliError::config("invalid config file"))),
            EXIT_CONFIG
        );
        assert_eq!(
            categorize_error(&anyhow::anyhow!(CliError::config("provider not found"))),
            EXIT_CONFIG
        );
    }

    #[test]
    fn test_categorize_anyhow_error_defaults_to_exit_error() {
        assert_eq!(
            categorize_error(&anyhow::anyhow!("Unknown error")),
            EXIT_ERROR
        );
        assert_eq!(
            categorize_error(&anyhow::anyhow!("API failure")),
            EXIT_ERROR
        );
        assert_eq!(
            categorize_error(&anyhow::anyhow!("Network timeout")),
            EXIT_ERROR
        );
    }
}
