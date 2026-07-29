//! LLM planner stub — always returns [`PlanError::PlannerDisabled`].

use async_trait::async_trait;

use crate::session::ReplanReason;

use super::template_service::{PlanContext, PlanError, PlanResult, PlanService};
use crate::dag::TemplateId;

/// Disabled LLM planner. Constructed only in tests or behind a future feature flag.
#[derive(Debug, Default, Clone, Copy)]
pub struct DisabledLlmPlanService;

#[async_trait]
impl PlanService for DisabledLlmPlanService {
    async fn plan(&self, _ctx: PlanContext) -> Result<PlanResult, PlanError> {
        Err(PlanError::PlannerDisabled)
    }

    async fn load_template(
        &self,
        _id: TemplateId,
        _ctx: PlanContext,
    ) -> Result<PlanResult, PlanError> {
        Err(PlanError::PlannerDisabled)
    }

    async fn replan(
        &self,
        _reason: ReplanReason,
        _ctx: PlanContext,
    ) -> Result<PlanResult, PlanError> {
        Err(PlanError::PlannerDisabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::{
        compiler_fingerprint_digest, policy_hash_digest, tool_versions_digest, TemplateId,
    };
    use crate::types::budget::Goal;
    use crate::types::ids::{DagId, RunId, SessionId};
    use crate::types::toolchain::ToolchainRecord;

    fn ctx() -> PlanContext {
        PlanContext {
            session_id: SessionId::new(),
            run_id: RunId::new(),
            dag_id: DagId::new(),
            goal: Goal {
                text: "x".into(),
                constraints: vec![],
                attachments: vec![],
            },
            template_override: None,
            policy_hash: policy_hash_digest(
                &crate::types::ids::ProfileId::new("default").unwrap(),
                &crate::types::budget::BudgetPolicy::default(),
            ),
            tool_versions: tool_versions_digest(&fixture_toolchain()),
            compiler_fingerprint: compiler_fingerprint_digest(
                &fixture_toolchain(),
                "x86_64-unknown-linux-gnu",
            ),
            prior_source: None,
            prior_proposal_artifact: None,
        }
    }

    fn fixture_toolchain() -> ToolchainRecord {
        ToolchainRecord {
            channel: "1.97.1".into(),
            rustc_version: "rustc 1.97.1 (test)".into(),
            cargo_version: "cargo 1.97.1 (test)".into(),
        }
    }

    #[tokio::test]
    async fn stub_disabled() {
        let s = DisabledLlmPlanService;
        assert!(matches!(
            s.plan(ctx()).await,
            Err(PlanError::PlannerDisabled)
        ));
        assert!(matches!(
            s.load_template(TemplateId::RepairLocalDiagnostic, ctx())
                .await,
            Err(PlanError::PlannerDisabled)
        ));
        assert!(matches!(
            s.replan(ReplanReason::UserRequested, ctx()).await,
            Err(PlanError::PlannerDisabled)
        ));
    }
}
