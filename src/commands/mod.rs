//! One module per `mono` subcommand.
//!
//! Each command owns its own logic and its own error vocabulary; callers
//! reach them through the re-exports in the crate root.

pub mod changelog;
pub mod ci;
pub mod doctor;
pub mod init;
pub mod list;
pub mod release;
