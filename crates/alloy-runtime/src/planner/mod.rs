//! PlanService: template selection, instantiation, CAS persist, PlanProduced (RFC-0009).
//!
//! Production callers construct [`TemplatePlanService::from_storage`] (or
//! [`TemplatePlanService::new`]) and inject `Arc<dyn PlanService>` into the
//! CLI / host (RFC-0015) — never into a capability worker (RFC-0013
//! AM-0009-1 / rule PW2: topology has exactly one writer).

mod llm_stub;
pub(crate) mod persist;
pub(crate) mod seed;
mod template_service;

pub use llm_stub::DisabledLlmPlanService;
pub use template_service::{
    PlanContext, PlanError, PlanProducedPayload, PlanResult, PlanService, PlanSource,
    TemplatePlanService,
};
