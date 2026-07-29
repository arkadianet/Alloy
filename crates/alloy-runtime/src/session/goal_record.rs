//! `RunRow.goal_json` envelope.

use serde::{Deserialize, Serialize};

use crate::types::budget::Goal;
use crate::types::ids::DagId;

/// Version of the trajectory record shape a run was created under
/// (research §7.11 item 7). `0` marks rows written before trajectory
/// identity existed.
pub const TRAJECTORY_SCHEMA_VERSION: u32 = 1;

/// Goal plus minted DAG id binding stored in `RunRow.goal_json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunGoalRecord {
    /// User goal.
    pub goal: Goal,
    /// Minted at `submit_goal`. DAG body persistence is RFC-0009.
    pub dag_id: DagId,
    /// Trajectory identity, minted beside `dag_id` at `submit_goal`.
    /// `None` only on rows written before this field existed.
    #[serde(default)]
    pub trajectory_id: Option<crate::types::ids::TrajectoryId>,
    /// [`TRAJECTORY_SCHEMA_VERSION`] at mint time; `0` on legacy rows.
    #[serde(default)]
    pub trajectory_schema: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::budget::Goal;

    #[test]
    fn ignores_unknown_fields() {
        let dag = DagId::new();
        let raw = serde_json::json!({
            "goal": { "text": "x", "constraints": [], "attachments": [] },
            "dag_id": dag.to_string(),
            "future_field": 1
        });
        let rec: RunGoalRecord = serde_json::from_value(raw).unwrap();
        assert_eq!(rec.goal.text, "x");
        assert_eq!(rec.dag_id, dag);
    }

    #[test]
    fn roundtrip() {
        let rec = RunGoalRecord {
            goal: Goal {
                text: "fix".into(),
                constraints: vec![],
                attachments: vec![],
            },
            dag_id: DagId::new(),
            trajectory_id: Some(crate::types::ids::TrajectoryId::new()),
            trajectory_schema: TRAJECTORY_SCHEMA_VERSION,
        };
        let v = serde_json::to_value(&rec).unwrap();
        let back: RunGoalRecord = serde_json::from_value(v).unwrap();
        assert_eq!(back, rec);
    }

    /// §7.11 item 7: rows written before trajectory identity existed still
    /// parse — `trajectory_id` reads `None`, schema reads `0`.
    #[test]
    fn legacy_goal_json_without_trajectory_fields_parses() {
        let dag = DagId::new();
        let raw = serde_json::json!({
            "goal": { "text": "x", "constraints": [], "attachments": [] },
            "dag_id": dag.to_string(),
        });
        let rec: RunGoalRecord = serde_json::from_value(raw).unwrap();
        assert_eq!(rec.trajectory_id, None);
        assert_eq!(rec.trajectory_schema, 0);
    }
}
