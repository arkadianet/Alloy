//! Session Manager & RunController (RFC-0003).
//!
//! [`SessionPlane`] owns session lifecycle, events, and budgets; run control lives in
//! [`RunController`]. Neither executes tools nor mutates DAG topology (V2 §5.2 / ADR F-22).
//! Durable truth is the RFC-0002 session/run rows plus the session event log.
//!
//! Author: arkadianet

mod gates;
mod goal_record;
mod inner;
mod map_err;
mod metrics;
mod plane;
mod profiles;
mod run_controller;
mod run_state;
mod service;
mod traits;

#[cfg(test)]
mod tests;

pub use goal_record::RunGoalRecord;
pub use metrics::SessionMetrics;
pub use plane::SessionPlane;
pub use run_state::RunControlState;
pub use traits::{
    clamp_events_page_limit, ReplanReason, RunController, Session, SessionService, MAX_EVENTS_PAGE,
};
