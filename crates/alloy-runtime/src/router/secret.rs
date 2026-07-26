//! Non-formatting API-key wrapper.

/// API-key material used only to construct an authorization header.
///
/// This type intentionally implements neither `Display`, serialization, nor
/// cloning so secrets cannot accidentally enter logs or event payloads.
pub struct SecretString {
    value: String,
}

impl SecretString {
    /// Wrap secret material without validation or logging.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
        }
    }

    /// Borrow secret material for authorization-header construction only.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.value
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_exposes_secret() {
        let secret = SecretString::new("private-material");
        let debug = format!("{secret:?}");
        assert_eq!(debug, "SecretString([REDACTED])");
        assert!(!debug.contains("private-material"));
    }
}
