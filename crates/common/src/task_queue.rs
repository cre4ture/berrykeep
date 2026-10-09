//! Best-effort task queue snapshots shared by server and client surfaces.
//!
//! These values are diagnostic observations, not scheduler guarantees. Producers
//! should prefer a cheap, slightly stale snapshot over taking a lock that can
//! delay the work being observed.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskQueueState {
    Idle,
    Waiting,
    Running,
    Backlogged,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskQueueEntry {
    pub id: String,
    pub label: String,
    pub pending: usize,
    pub active: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<usize>,
    pub state: TaskQueueState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl TaskQueueEntry {
    pub fn observed(
        id: impl Into<String>,
        label: impl Into<String>,
        pending: usize,
        active: usize,
        capacity: Option<usize>,
        detail: Option<String>,
    ) -> Self {
        let state = match (pending, active, capacity) {
            (0, 0, _) => TaskQueueState::Idle,
            (0, _, _) => TaskQueueState::Running,
            (_, 0, _) => TaskQueueState::Waiting,
            (_, _, Some(capacity)) if active >= capacity => TaskQueueState::Backlogged,
            _ => TaskQueueState::Running,
        };
        Self {
            id: id.into(),
            label: label.into(),
            pending,
            active,
            capacity,
            state,
            detail,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskQueueSnapshot {
    pub generated_at_unix_ms: u64,
    pub queues: Vec<TaskQueueEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClusterTaskQueueNodeSnapshot {
    pub node_id: String,
    pub queues: Vec<TaskQueueEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnavailableTaskQueueNode {
    pub node_id: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClusterTaskQueueSnapshot {
    pub generated_at_unix_ms: u64,
    pub nodes: Vec<ClusterTaskQueueNodeSnapshot>,
    pub unavailable_nodes: Vec<UnavailableTaskQueueNode>,
}

#[cfg(test)]
mod tests {
    use super::{TaskQueueEntry, TaskQueueState};

    #[test]
    fn derives_an_honest_state_from_observed_counts() {
        assert_eq!(entry(0, 0, Some(2)).state, TaskQueueState::Idle);
        assert_eq!(entry(0, 1, Some(2)).state, TaskQueueState::Running);
        assert_eq!(entry(2, 0, Some(2)).state, TaskQueueState::Waiting);
        assert_eq!(entry(2, 2, Some(2)).state, TaskQueueState::Backlogged);
        assert_eq!(entry(2, 1, Some(2)).state, TaskQueueState::Running);
    }

    fn entry(pending: usize, active: usize, capacity: Option<usize>) -> TaskQueueEntry {
        TaskQueueEntry::observed("test", "Test", pending, active, capacity, None)
    }
}
