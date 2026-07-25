//! MVP profile id validation.

use crate::error::SessionError;
use crate::types::ids::ProfileId;

/// MVP profile catalog strings.
pub const MVP_PROFILES: &[&str] = &["default", "autonomous", "readonly"];

/// Reject unsupported profile ids at session create.
pub fn validate_mvp_profile(profile: &ProfileId) -> Result<(), SessionError> {
    if MVP_PROFILES.contains(&profile.as_str()) {
        Ok(())
    } else {
        Err(SessionError::Invalid(format!(
            "unsupported profile: {}",
            profile.as_str()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_mvp() {
        for id in MVP_PROFILES {
            validate_mvp_profile(&ProfileId::new(*id).unwrap()).unwrap();
        }
    }

    #[test]
    fn rejects_other() {
        let err = validate_mvp_profile(&ProfileId::new("custom").unwrap()).unwrap_err();
        assert!(matches!(err, SessionError::Invalid(_)));
    }
}
