use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::error::DenError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxState {
    Pending,
    Starting,
    Running,
    /// Container died unexpectedly (OOM, crash). Resumable — not terminal.
    Defunct,
    Completed,
    Failed,
    Timeout,
}

impl SandboxState {
    pub fn valid_transitions(self) -> &'static [SandboxState] {
        use SandboxState::*;
        match self {
            Pending => &[Starting, Failed],
            Starting => &[Running, Failed],
            Running => &[Completed, Failed, Timeout, Defunct],
            Defunct => &[], // resume resets state directly, not via transition()
            Completed | Failed | Timeout => &[],
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Timeout)
    }

    /// Returns true if the sandbox can accept exec requests.
    pub fn is_usable(self) -> bool {
        self == Self::Running
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Sandbox {
    pub id: String,
    pub container_id: String,
    pub volume_name: String,
    pub tier: String,
    pub state: SandboxState,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub last_activity: DateTime<Utc>,
    pub timeout_secs: u64,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
    /// Host bind mounts for dedicated sandboxes (e.g. "/host/path:/container/path:rw").
    /// Stored so resume can recreate the container with the same mounts.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bind_mounts: Vec<String>,
    /// Max concurrent exec requests. Set from tier config at claim time.
    #[serde(skip)]
    pub max_concurrent_execs: usize,
}

impl Sandbox {
    pub fn new(
        id: String,
        container_id: String,
        volume_name: String,
        tier: String,
        timeout_secs: u64,
        metadata: HashMap<String, String>,
        bind_mounts: Vec<String>,
        max_concurrent_execs: usize,
    ) -> Self {
        let now = Utc::now();
        Self {
            id,
            container_id,
            volume_name,
            tier,
            state: SandboxState::Pending,
            created_at: now,
            started_at: None,
            last_activity: now,
            timeout_secs,
            metadata,
            bind_mounts,
            max_concurrent_execs,
        }
    }

    pub fn transition(&mut self, to: SandboxState) -> Result<(), DenError> {
        if self.state.valid_transitions().contains(&to) {
            self.state = to;
            self.last_activity = Utc::now();
            if to == SandboxState::Running {
                self.started_at = Some(Utc::now());
            }
            tracing::info!(sandbox = %self.id, from = ?self.state, to = ?to, "state transition");
            Ok(())
        } else {
            Err(DenError::InvalidState {
                id: self.id.clone(),
                current: self.state,
                operation: format!("transition to {to:?}"),
            })
        }
    }

    pub fn touch(&mut self) {
        self.last_activity = Utc::now();
    }

    pub fn is_expired(&self) -> bool {
        if let Some(started) = self.started_at {
            let elapsed = Utc::now().signed_duration_since(started);
            elapsed.num_seconds() as u64 >= self.timeout_secs
        } else {
            false
        }
    }

    pub fn is_idle(&self, idle_timeout_secs: u64) -> bool {
        let elapsed = Utc::now().signed_duration_since(self.last_activity);
        elapsed.num_seconds() as u64 >= idle_timeout_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sandbox() -> Sandbox {
        Sandbox::new(
            "test-1".into(),
            "container-abc".into(),
            "den-vol-test-1".into(),
            "standard".into(),
            60,
            HashMap::new(),
            Vec::new(),
            4,
        )
    }

    #[test]
    fn valid_transitions() {
        let mut s = make_sandbox();
        assert_eq!(s.state, SandboxState::Pending);

        assert!(s.transition(SandboxState::Starting).is_ok());
        assert_eq!(s.state, SandboxState::Starting);

        assert!(s.transition(SandboxState::Running).is_ok());
        assert_eq!(s.state, SandboxState::Running);
        assert!(s.started_at.is_some());

        assert!(s.transition(SandboxState::Completed).is_ok());
        assert!(s.state.is_terminal());
    }

    #[test]
    fn invalid_transition_from_pending_to_running() {
        let mut s = make_sandbox();
        assert!(s.transition(SandboxState::Running).is_err());
    }

    #[test]
    fn invalid_transition_from_terminal() {
        let mut s = make_sandbox();
        s.state = SandboxState::Completed;
        assert!(s.transition(SandboxState::Running).is_err());
    }

    #[test]
    fn pending_to_failed() {
        let mut s = make_sandbox();
        assert!(s.transition(SandboxState::Failed).is_ok());
        assert!(s.state.is_terminal());
    }

    #[test]
    fn running_to_timeout() {
        let mut s = make_sandbox();
        s.state = SandboxState::Running;
        assert!(s.transition(SandboxState::Timeout).is_ok());
    }

    #[test]
    fn running_to_defunct() {
        let mut s = make_sandbox();
        s.state = SandboxState::Running;
        assert!(s.transition(SandboxState::Defunct).is_ok());
        assert_eq!(s.state, SandboxState::Defunct);
        assert!(!s.state.is_terminal()); // resumable
        assert!(!s.state.is_usable());   // not exec-able
    }

    #[test]
    fn defunct_is_not_terminal() {
        assert!(!SandboxState::Defunct.is_terminal());
    }

    #[test]
    fn is_expired_false_when_not_started() {
        let s = make_sandbox();
        assert!(!s.is_expired());
    }

    #[test]
    fn touch_updates_last_activity() {
        let mut s = make_sandbox();
        let before = s.last_activity;
        std::thread::sleep(std::time::Duration::from_millis(10));
        s.touch();
        assert!(s.last_activity > before);
    }
}
