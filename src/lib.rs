#![deny(clippy::all)]
#![allow(
    clippy::too_many_arguments,
    clippy::if_same_then_else,
    clippy::never_loop,
    clippy::empty_line_after_doc_comments,
    clippy::large_enum_variant,
    clippy::type_complexity,
    clippy::format_push_string,
    clippy::too_many_lines,
    clippy::fn_params_excessive_bools,
    clippy::case_sensitive_file_extension_comparisons,
    clippy::too_long_first_doc_paragraph,
    clippy::used_underscore_items,
    clippy::single_match,
    clippy::match_wildcard_for_single_variants,
    clippy::branches_sharing_code,
    clippy::significant_drop_tightening,
    clippy::unused_self,
    clippy::needless_continue,
    clippy::map_unwrap_or
)]

pub mod cli;
pub mod core;
pub mod error;
pub mod exit_codes;
pub mod providers;
pub mod services;
pub mod storage;
pub mod terminal;

#[cfg(test)]
pub mod test_support;

// Re-export crate as `sned` for use in main.rs and tests
pub use crate as sned;
