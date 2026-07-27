//! PlanService: template selection, instantiation, CAS persist, PlanProduced (RFC-0009).

mod llm_stub;
mod template_service;

pub use llm_stub::DisabledLlmPlanService;
pub use template_service::{
    PlanContext, PlanError, PlanProducedPayload, PlanResult, PlanService, TemplatePlanService,
};
