#![deny(clippy::all)]
#![allow(
    clippy::too_many_arguments,
    clippy::if_same_then_else,
    clippy::never_loop,
    clippy::empty_line_after_doc_comments,
    clippy::large_enum_variant,
    clippy::type_complexity
)]

pub mod cli;
pub mod core;
pub mod error;
pub mod exit_codes;
pub mod providers;
pub mod services;
pub mod storage;
pub mod terminal;

// Re-export crate as `sned` for use in main.rs and tests
pub use crate as sned;
