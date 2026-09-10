//! `monore doctor` — check that a directory holds a healthy repository.

use std::error::Error as StdError;
use std::fmt;
use std::path::Path;

/// Check that `dir` holds a healthy monorepo.
///
/// Not implemented yet. It fails loudly instead of reporting a success it
/// never verified.
pub fn doctor(_dir: &Path) -> Result<(), DoctorError> {
    Err(DoctorError::NotImplemented)
}

/// Expected failures of [`doctor`].
#[derive(Debug)]
pub enum DoctorError {
    /// The check is not implemented yet.
    NotImplemented,
}

impl fmt::Display for DoctorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotImplemented => write!(f, "doctor is not implemented yet"),
        }
    }
}

impl StdError for DoctorError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn reports_that_it_is_not_implemented() {
        let temp = TempDir::new();

        assert!(matches!(
            doctor(temp.path()),
            Err(DoctorError::NotImplemented)
        ));
    }
}
