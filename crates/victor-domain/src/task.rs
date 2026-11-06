use serde::{Deserialize, Serialize};

use crate::error::DomainError;
use crate::UnixMs;

/// Stany zadania z §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Received,
    Interpreted,
    Planned,
    AwaitingApproval,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl TaskState {
    pub const ALL: [TaskState; 8] = [
        TaskState::Received,
        TaskState::Interpreted,
        TaskState::Planned,
        TaskState::AwaitingApproval,
        TaskState::Running,
        TaskState::Succeeded,
        TaskState::Failed,
        TaskState::Cancelled,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::Received => "received",
            TaskState::Interpreted => "interpreted",
            TaskState::Planned => "planned",
            TaskState::AwaitingApproval => "awaiting_approval",
            TaskState::Running => "running",
            TaskState::Succeeded => "succeeded",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
        }
    }

    /// Stan końcowy nie może już przejść dalej.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
        )
    }

    /// Graf przejść jest zamknięty: wszystko, czego nie ma poniżej, jest błędem.
    ///
    /// `Planned -> Running` jest dozwolone tylko dla planów R0, które nie wymagają
    /// zgody; dla R1+ runtime musi najpierw przejść przez `AwaitingApproval`.
    pub fn can_transition_to(self, next: TaskState) -> bool {
        use TaskState::*;
        matches!(
            (self, next),
            (Received, Interpreted | Cancelled)
                | (Interpreted, Planned | Cancelled)
                | (Planned, AwaitingApproval | Running | Cancelled)
                | (AwaitingApproval, Running | Cancelled)
                | (Running, Succeeded | Failed | Cancelled)
        )
    }
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(String);

impl TaskId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub state: TaskState,
    pub intent: Option<String>,
    pub summary: Option<String>,
    pub created_at_unix_ms: UnixMs,
    pub updated_at_unix_ms: UnixMs,
}

impl Task {
    pub fn new(id: TaskId, now: UnixMs) -> Self {
        Self {
            id,
            state: TaskState::Received,
            intent: None,
            summary: None,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
        }
    }

    pub fn transition_to(&mut self, next: TaskState, now: UnixMs) -> Result<(), DomainError> {
        if !self.state.can_transition_to(next) {
            return Err(DomainError::InvalidTransition {
                from: self.state.as_str().to_string(),
                to: next.as_str().to_string(),
            });
        }
        self.state = next;
        self.updated_at_unix_ms = now;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_reaches_success() {
        let mut task = Task::new(TaskId::new("t1"), 0);
        for (state, at) in [
            (TaskState::Interpreted, 1),
            (TaskState::Planned, 2),
            (TaskState::AwaitingApproval, 3),
            (TaskState::Running, 4),
            (TaskState::Succeeded, 5),
        ] {
            task.transition_to(state, at).unwrap();
        }
        assert_eq!(task.state, TaskState::Succeeded);
        assert_eq!(task.updated_at_unix_ms, 5);
        assert!(task.state.is_terminal());
    }

    #[test]
    fn r0_plan_may_run_without_approval() {
        assert!(TaskState::Planned.can_transition_to(TaskState::Running));
    }

    #[test]
    fn rejected_approval_finishes_as_cancelled() {
        assert!(TaskState::AwaitingApproval.can_transition_to(TaskState::Cancelled));
        assert!(!TaskState::AwaitingApproval.can_transition_to(TaskState::Succeeded));
    }

    #[test]
    fn terminal_states_do_not_transition() {
        for terminal in [
            TaskState::Succeeded,
            TaskState::Failed,
            TaskState::Cancelled,
        ] {
            assert!(terminal.is_terminal());
            for next in TaskState::ALL {
                assert!(!terminal.can_transition_to(next));
            }
        }
    }

    #[test]
    fn no_state_transitions_to_itself() {
        for state in TaskState::ALL {
            assert!(!state.can_transition_to(state));
        }
    }

    #[test]
    fn invalid_transition_is_reported_with_both_states() {
        let mut task = Task::new(TaskId::new("t1"), 0);
        let err = task.transition_to(TaskState::Running, 1).unwrap_err();
        assert_eq!(
            err,
            DomainError::InvalidTransition {
                from: "received".into(),
                to: "running".into()
            }
        );
        assert_eq!(task.state, TaskState::Received);
        assert_eq!(task.updated_at_unix_ms, 0);
    }
}
