//! Context assembly errors (RFC-0012 §3.7, §9).
//!
//! There is deliberately no `Graph` and no `Store` variant, and no
//! `From<GraphError>` / `From<StoreError>` impl: a graph or store failure is
//! a [`super::Degradation`], not an error (rule E1, T-CI8).

use crate::types::ids::SummaryId;

use super::types::DomainId;

/// Context assembly failure. Every variant is a **caller** error or a
/// genuinely impossible request — never a degraded input (E1).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ContextError {
    /// `token_budget == 0`, or the effective budget cannot hold the system
    /// frame plus the pinned goal and `must_include` items (rule E5).
    #[error("budget too small: need >= {needed} estimated tokens, have {have}")]
    BudgetTooSmall {
        /// Minimum viable estimate.
        needed: usize,
        /// Effective budget.
        have: usize,
    },
    /// A `must_include` item does not fit even alone (rule B11, E6).
    #[error("must-include does not fit: {0}")]
    MustIncludeTooLarge(String),
    /// A `must_include` item does not exist (rule E7).
    #[error("must-include not found: {0}")]
    MustIncludeNotFound(String),
    /// The request is malformed (absolute path, empty capability, bad range).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Profile weights or limits are invalid (rule D2).
    #[error("invalid profile: {0}")]
    InvalidProfile(String),
    /// `mark_stale` named an unknown projection (§8.2).
    #[error("no such summary: {0}")]
    SummaryNotFound(SummaryId),
    /// `compact` named a reserved domain (rule A12).
    #[error("domain not live: {0:?}")]
    DomainNotLive(DomainId),
    /// The assembled pack has no user content (rule A15).
    #[error("empty prompt: no user content could be assembled")]
    EmptyPrompt,
    /// Internal invariant violation, e.g. the post-assembly budget assertion.
    #[error("internal: {0}")]
    Internal(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    // T1g — E9: every variant is a caller/configuration error; there is no
    // graph or store variant. Exhaustive in-crate match proves it at
    // compile time.
    #[test]
    fn context_error_variants_are_all_caller_errors() {
        let variants = [
            ContextError::BudgetTooSmall { needed: 1, have: 0 },
            ContextError::MustIncludeTooLarge("x".into()),
            ContextError::MustIncludeNotFound("x".into()),
            ContextError::InvalidRequest("x".into()),
            ContextError::InvalidProfile("x".into()),
            ContextError::SummaryNotFound(crate::types::ids::SummaryId::new()),
            ContextError::DomainNotLive(DomainId::Conversation),
            ContextError::EmptyPrompt,
            ContextError::Internal("x".into()),
        ];
        for v in variants {
            // A degraded input never surfaces here (E1): no arm names a
            // graph or store failure.
            let caller_error = match v {
                ContextError::BudgetTooSmall { .. }
                | ContextError::MustIncludeTooLarge(_)
                | ContextError::MustIncludeNotFound(_)
                | ContextError::InvalidRequest(_)
                | ContextError::InvalidProfile(_)
                | ContextError::SummaryNotFound(_)
                | ContextError::DomainNotLive(_)
                | ContextError::EmptyPrompt
                | ContextError::Internal(_) => true,
            };
            assert!(caller_error);
        }
    }
}
