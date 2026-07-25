//! Session and RunController trait signatures (impl in RFC-0003).

mod traits;

pub use traits::{
    clamp_events_page_limit, ReplanReason, RunController, Session, SessionService, MAX_EVENTS_PAGE,
};
