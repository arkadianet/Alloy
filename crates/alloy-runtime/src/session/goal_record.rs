//! `RunRow.goal_json` envelope.

use serde::{Deserialize, Serialize};

use crate::types::budget::Goal;
use crate::types::ids::DagId;

/// Goal plus minted DAG id binding stored in `RunRow.goal_json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunGoalRecord {
    /// User goal.
    pub goal: Goal,
    /// Minted at `submit_goal`. DAG body persistence is RFC-0009.
    pub dag_id: DagId,
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
        };
        let v = serde_json::to_value(&rec).unwrap();
        let back: RunGoalRecord = serde_json::from_value(v).unwrap();
        assert_eq!(back, rec);
    }
}
