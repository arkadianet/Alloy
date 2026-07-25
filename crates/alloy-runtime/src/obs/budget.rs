//! Budget warning helper (RFC-0004 §3.13 / §6.7).

use crate::obs::cost::{BudgetCheck, SharedCostMeter};
use crate::obs::error::ObsError;
use crate::session::SessionPlane;
use crate::types::budget::BudgetPolicy;
use crate::types::ids::{EventSeq, RunId, SessionId};

/// If the meter is exhausted, invoke [`SessionPlane::signal_budget_warning`].
///
/// Returns `Ok(None)` when under budget; `Ok(Some(seq))` when a warning was appended.
///
/// Does **not** hold the meter lock across the session-plane await.
pub async fn maybe_signal_budget_warning(
    plane: &SessionPlane,
    session: SessionId,
    run: Option<RunId>,
    meter: &SharedCostMeter,
    policy: &BudgetPolicy,
) -> Result<Option<EventSeq>, ObsError> {
    let (check, snapshot) = meter.with_mut(|m| (m.check_budget(policy), m.to_budget_snapshot()));
    if !check.is_exhausted() {
        return Ok(None);
    }
    let message = match check {
        BudgetCheck::Ok => unreachable!("checked is_exhausted"),
        BudgetCheck::TokensExhausted => "budget exhausted: tokens",
        BudgetCheck::UsdExhausted => "budget exhausted: usd",
        BudgetCheck::TokensAndUsdExhausted => "budget exhausted: tokens and usd",
    };
    tracing::warn!(%session, ?run, ?check, "budget warning helper invoked");
    let seq = plane
        .signal_budget_warning(session, run, snapshot, message)
        .await?;
    Ok(Some(seq))
}
