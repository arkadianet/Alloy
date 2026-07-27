//! PlanService: template selection, instantiation, CAS persist, PlanProduced (RFC-0009).
//!
//! Production callers construct [`TemplatePlanService::from_storage`] (or
//! [`TemplatePlanService::new`]) and inject `Arc<dyn PlanService>` into
//! PlanningWorker (RFC-0013) / CLI (RFC-0015).

mod llm_stub;
mod template_service;

pub use llm_stub::DisabledLlmPlanService;
pub use template_service::{
    PlanContext, PlanError, PlanProducedPayload, PlanResult, PlanService, TemplatePlanService,
};
