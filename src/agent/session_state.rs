//! Session state metadata tracking.
//!
//! Tracks the current state of an agent session with rich metadata
//! for webhook/UI integration. Following Claude Code's sessionState.ts pattern.

use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Current state of an agent session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Session is idle, waiting for user input.
    Idle,
    /// Agent is actively processing (LLM call or tool execution).
    Running,
    /// Agent needs user action (permission approval, input needed).
    RequiresAction,
    /// Session has ended.
    Completed,
    /// Session encountered an unrecoverable error.
    Error,
}

/// Details about what action the agent requires from the user.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RequiresActionDetails {
    pub tool_name: Option<String>,
    pub action_description: String,
}

/// Rich session metadata for external consumers (webhooks, UI).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionMetadata {
    pub status: SessionStatus,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub current_tool: Option<String>,
    pub turn_count: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub requires_action: Option<RequiresActionDetails>,
    pub elapsed_secs: u64,
    pub last_error: Option<String>,
}

/// Thread-safe session state tracker.
pub struct SessionStateTracker {
    inner: Arc<Mutex<SessionStateInner>>,
}

struct SessionStateInner {
    status: SessionStatus,
    model: Option<String>,
    provider: Option<String>,
    current_tool: Option<String>,
    turn_count: u32,
    total_input_tokens: u64,
    total_output_tokens: u64,
    requires_action: Option<RequiresActionDetails>,
    started_at: Instant,
    last_error: Option<String>,
}

impl SessionStateTracker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(SessionStateInner {
                status: SessionStatus::Idle,
                model: None,
                provider: None,
                current_tool: None,
                turn_count: 0,
                total_input_tokens: 0,
                total_output_tokens: 0,
                requires_action: None,
                started_at: Instant::now(),
                last_error: None,
            })),
        }
    }

    /// Transition to running state (LLM call started).
    pub fn set_running(&self, provider: &str, model: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.status = SessionStatus::Running;
        inner.provider = Some(provider.to_string());
        inner.model = Some(model.to_string());
        inner.current_tool = None;
        inner.requires_action = None;
    }

    /// Record that a tool is currently executing.
    pub fn set_tool_running(&self, tool_name: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.current_tool = Some(tool_name.to_string());
    }

    /// Transition to idle state (turn completed).
    pub fn set_idle(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.status = SessionStatus::Idle;
        inner.current_tool = None;
        inner.turn_count += 1;
        inner.requires_action = None;
    }

    /// Transition to requires_action state (needs user approval).
    pub fn set_requires_action(&self, details: RequiresActionDetails) {
        let mut inner = self.inner.lock().unwrap();
        inner.status = SessionStatus::RequiresAction;
        inner.requires_action = Some(details);
    }

    /// Record token usage from an LLM call.
    pub fn add_tokens(&self, input: u64, output: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.total_input_tokens += input;
        inner.total_output_tokens += output;
    }

    /// Record an error.
    pub fn set_error(&self, error: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.status = SessionStatus::Error;
        inner.last_error = Some(error.to_string());
    }

    /// Set completed state.
    pub fn set_completed(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.status = SessionStatus::Completed;
    }

    /// Get current session metadata snapshot.
    pub fn metadata(&self) -> SessionMetadata {
        let inner = self.inner.lock().unwrap();
        SessionMetadata {
            status: inner.status.clone(),
            model: inner.model.clone(),
            provider: inner.provider.clone(),
            current_tool: inner.current_tool.clone(),
            turn_count: inner.turn_count,
            total_input_tokens: inner.total_input_tokens,
            total_output_tokens: inner.total_output_tokens,
            requires_action: inner.requires_action.clone(),
            elapsed_secs: inner.started_at.elapsed().as_secs(),
            last_error: inner.last_error.clone(),
        }
    }
}

impl Default for SessionStateTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for SessionStateTracker {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_idle() {
        let tracker = SessionStateTracker::new();
        assert_eq!(tracker.metadata().status, SessionStatus::Idle);
    }

    #[test]
    fn transition_to_running() {
        let tracker = SessionStateTracker::new();
        tracker.set_running("anthropic", "claude-sonnet");
        let meta = tracker.metadata();
        assert_eq!(meta.status, SessionStatus::Running);
        assert_eq!(meta.provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn tool_running_tracked() {
        let tracker = SessionStateTracker::new();
        tracker.set_running("anthropic", "claude-sonnet");
        tracker.set_tool_running("file_read");
        assert_eq!(
            tracker.metadata().current_tool.as_deref(),
            Some("file_read")
        );
    }

    #[test]
    fn idle_increments_turn_count() {
        let tracker = SessionStateTracker::new();
        tracker.set_running("a", "b");
        tracker.set_idle();
        tracker.set_running("a", "b");
        tracker.set_idle();
        assert_eq!(tracker.metadata().turn_count, 2);
    }

    #[test]
    fn token_tracking() {
        let tracker = SessionStateTracker::new();
        tracker.add_tokens(1000, 500);
        tracker.add_tokens(2000, 800);
        let meta = tracker.metadata();
        assert_eq!(meta.total_input_tokens, 3000);
        assert_eq!(meta.total_output_tokens, 1300);
    }

    #[test]
    fn requires_action_state() {
        let tracker = SessionStateTracker::new();
        tracker.set_requires_action(RequiresActionDetails {
            tool_name: Some("shell".into()),
            action_description: "Approve command execution".into(),
        });
        let meta = tracker.metadata();
        assert_eq!(meta.status, SessionStatus::RequiresAction);
        assert!(meta.requires_action.is_some());
    }
}
