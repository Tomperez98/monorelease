//! One module per `monore` subcommand.
//!
//! Each command owns its own logic and its own error vocabulary; callers
//! reach them through the re-exports in the crate root.

pub mod doctor;
pub mod init;
