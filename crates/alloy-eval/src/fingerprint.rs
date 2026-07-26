//! Exact-serde request fingerprints for scripted provider keying.

use alloy_runtime::{CompletionRequest, Digest};
use serde::{Deserialize, Serialize};

use crate::error::EvalError;

/// SHA-256 digest of the canonical JSON encoding of a [`CompletionRequest`].
///
/// Canonical encoding is the exact `serde_json::to_vec` output for the stored
/// request. Digests are lowercase hex via [`Digest::as_hex`].
///
/// Ownership: owned digest wrapper. Failure semantics: [`of`](Self::of) is
/// infallible; [`from_hex`](Self::from_hex) returns [`EvalError::Manifest`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestFingerprint(Digest);

impl RequestFingerprint {
    /// Compute fingerprint for `request`.
    ///
    /// Uses exact `serde_json::to_vec` bytes with no Unicode normalization,
    /// trimming, case-folding, or key reordering.
    #[must_use]
    pub fn of(request: &CompletionRequest) -> Self {
        let bytes = serde_json::to_vec(request)
            .expect("CompletionRequest contains only infallibly serializable values");
        Self(Digest::sha256(&bytes))
    }

    /// Parse a 64-char lowercase hex digest; reject otherwise.
    ///
    /// Failure: [`EvalError::Manifest`] for wrong length, uppercase, or non-hex.
    pub fn from_hex(s: impl AsRef<str>) -> Result<Self, EvalError> {
        let s = s.as_ref();
        Digest::try_from_hex(s)
            .map(Self)
            .map_err(|_| EvalError::Manifest(format!("invalid fingerprint hex: {s}")))
    }

    /// Borrow the underlying digest.
    #[must_use]
    pub fn as_digest(&self) -> &Digest {
        &self.0
    }

    /// Borrow the lowercase hex string.
    #[must_use]
    pub fn as_hex(&self) -> &str {
        self.0.as_hex()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::{ChatMessage, ChatRole, ResponseFormat, ToolChoice};

    fn empty_request() -> CompletionRequest {
        CompletionRequest {
            messages: vec![],
            tools: vec![],
            tool_choice: ToolChoice::None,
            response_format: ResponseFormat::Text,
            temperature: None,
            max_output_tokens: None,
        }
    }

    #[test]
    fn fingerprint_empty_request_golden() {
        let req = empty_request();
        let bytes = serde_json::to_vec(&req).unwrap();
        let expected = b"{\"messages\":[],\"tools\":[],\"tool_choice\":\"none\",\"response_format\":\"text\",\"temperature\":null,\"max_output_tokens\":null}";
        assert_eq!(bytes.as_slice(), expected);
        assert_eq!(
            RequestFingerprint::of(&req).as_hex(),
            "71ab8ab13b7cb4a68d7727e6268d8793fc4f41506cac57ef15cb7c1931ef7d36"
        );
    }

    #[test]
    fn fingerprint_one_simple_message_golden() {
        let req = CompletionRequest {
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".into(),
            }],
            tools: vec![],
            tool_choice: ToolChoice::None,
            response_format: ResponseFormat::Text,
            temperature: None,
            max_output_tokens: None,
        };
        let bytes = serde_json::to_vec(&req).unwrap();
        let expected = b"{\"messages\":[{\"role\":\"user\",\"content\":\"hello\"}],\"tools\":[],\"tool_choice\":\"none\",\"response_format\":\"text\",\"temperature\":null,\"max_output_tokens\":null}";
        assert_eq!(bytes.as_slice(), expected);
        assert_eq!(
            RequestFingerprint::of(&req).as_hex(),
            "4e68ffe37fd31000068a317bf27e389a0cb8f9d9a01031f6d42cd9e8559e7d05"
        );
    }

    #[test]
    fn fingerprint_exact_utf8_no_normalization() {
        let mut a = empty_request();
        a.messages.push(ChatMessage {
            role: ChatRole::User,
            content: "é".into(), // U+00E9
        });
        let mut b = empty_request();
        b.messages.push(ChatMessage {
            role: ChatRole::User,
            content: "e\u{0301}".into(), // e + combining acute
        });
        assert_ne!(
            RequestFingerprint::of(&a).as_hex(),
            RequestFingerprint::of(&b).as_hex()
        );

        let mut spaced = empty_request();
        spaced.messages.push(ChatMessage {
            role: ChatRole::User,
            content: " hello ".into(),
        });
        let mut trimmed = empty_request();
        trimmed.messages.push(ChatMessage {
            role: ChatRole::User,
            content: "hello".into(),
        });
        assert_ne!(
            RequestFingerprint::of(&spaced).as_hex(),
            RequestFingerprint::of(&trimmed).as_hex()
        );
    }

    #[test]
    fn fingerprint_from_hex_validation() {
        assert!(RequestFingerprint::from_hex(
            "71ab8ab13b7cb4a68d7727e6268d8793fc4f41506cac57ef15cb7c1931ef7d36"
        )
        .is_ok());
        assert!(matches!(
            RequestFingerprint::from_hex(
                "71AB8AB13B7CB4A68D7727E6268D8793FC4F41506CAC57EF15CB7C1931EF7D36"
            ),
            Err(EvalError::Manifest(_))
        ));
        assert!(matches!(
            RequestFingerprint::from_hex("abcd"),
            Err(EvalError::Manifest(_))
        ));
        assert!(matches!(
            RequestFingerprint::from_hex(
                "71ab8ab13b7cb4a68d7727e6268d8793fc4f41506cac57ef15cb7c1931ef7d3g"
            ),
            Err(EvalError::Manifest(_))
        ));
    }
}
