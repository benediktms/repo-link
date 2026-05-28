//! `WorkspaceName` newtype with validation.

use domain_core::{DomainError, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceName(String);

impl WorkspaceName {
    pub fn new(s: impl Into<String>) -> Result<Self> {
        let s = s.into();
        let trimmed = s.trim();
        if trimmed.is_empty() || trimmed.len() > 128 {
            return Err(DomainError::validation(
                "workspace name must be 1..=128 chars",
            ));
        }
        if !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' '))
        {
            return Err(DomainError::validation(
                "workspace name may only contain ascii alphanumerics, dash, underscore, space",
            ));
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rejects_blank() {
        assert!(WorkspaceName::new("   ").is_err());
    }

    #[test]
    fn name_rejects_funky_chars() {
        assert!(WorkspaceName::new("hi/there").is_err());
    }
}
