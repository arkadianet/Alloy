//! Content hashing helpers (RFC-0004 §3.10).

use crate::types::ids::Digest;

/// SHA-256 lowercase hex via [`Digest::sha256`].
#[must_use]
pub fn hash_content(bytes: &[u8]) -> Digest {
    Digest::sha256(bytes)
}

/// Hash a prompt string (UTF-8 bytes).
#[must_use]
pub fn hash_prompt(prompt: &str) -> Digest {
    Digest::sha256(prompt.as_bytes())
}

/// Hash a tool body string (UTF-8 bytes).
#[must_use]
pub fn hash_tool_body(body: &str) -> Digest {
    Digest::sha256(body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ids::Digest;

    #[test]
    fn hash_prompt_stable() {
        let a = hash_prompt("hello");
        let b = hash_prompt("hello");
        assert_eq!(a, b);
        assert_eq!(a, Digest::sha256(b"hello"));
    }

    #[test]
    fn hash_tool_body_stable() {
        let a = hash_tool_body("{}");
        let b = hash_tool_body("{}");
        assert_eq!(a, b);
        assert_eq!(a, Digest::sha256(b"{}"));
    }
}
