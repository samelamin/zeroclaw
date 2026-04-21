use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;

use crate::channels::traits::ChannelMessage;
use crate::providers::traits::{ChatMessage, ChatResponse};
use crate::tools::traits::ToolResult;

/// Result of a modifying hook — continue with (possibly modified) data, or cancel.
#[derive(Debug, Clone)]
pub enum HookResult<T> {
    Continue(T),
    Cancel(String),
    /// Hard deny — cannot be overridden by any privilege escalation or bypass mode.
    /// Used for security-critical blocks (e.g., prompt injection detected in tool output,
    /// team budget exceeded). Analogous to Claude Code's exit code 2 enforcement.
    HardDeny(String),
}

impl<T> HookResult<T> {
    /// Returns `true` for both `Cancel` and `HardDeny` (any kind of cancellation).
    pub fn is_cancel(&self) -> bool {
        matches!(self, HookResult::Cancel(_) | HookResult::HardDeny(_))
    }

    /// Returns `true` only for the non-overridable `HardDeny` variant.
    pub fn is_hard_deny(&self) -> bool {
        matches!(self, HookResult::HardDeny(_))
    }

    /// Returns `true` only for a soft `Cancel` (not `HardDeny`).
    pub fn is_soft_cancel(&self) -> bool {
        matches!(self, HookResult::Cancel(_))
    }

    /// Returns the reason string for `Cancel` or `HardDeny`, or `None` for `Continue`.
    pub fn reason(&self) -> Option<&str> {
        match self {
            HookResult::Cancel(r) | HookResult::HardDeny(r) => Some(r),
            HookResult::Continue(_) => None,
        }
    }
}

/// Trait for hook handlers. All methods have default no-op implementations.
/// Implement only the events you care about.
#[async_trait]
pub trait HookHandler: Send + Sync {
    fn name(&self) -> &str;
    fn priority(&self) -> i32 {
        0
    }

    // --- Void hooks (parallel, fire-and-forget) ---
    async fn on_gateway_start(&self, _host: &str, _port: u16) {}
    async fn on_gateway_stop(&self) {}
    async fn on_session_start(&self, _session_id: &str, _channel: &str) {}
    async fn on_session_end(&self, _session_id: &str, _channel: &str) {}
    async fn on_llm_input(&self, _messages: &[ChatMessage], _model: &str) {}
    async fn on_llm_output(&self, _response: &ChatResponse) {}
    async fn on_after_tool_call(&self, _tool: &str, _result: &ToolResult, _duration: Duration) {}
    async fn on_message_sent(&self, _channel: &str, _recipient: &str, _content: &str) {}
    async fn on_heartbeat_tick(&self) {}

    // --- Modifying hooks (sequential by priority, can cancel) ---
    async fn before_model_resolve(
        &self,
        provider: String,
        model: String,
    ) -> HookResult<(String, String)> {
        HookResult::Continue((provider, model))
    }

    async fn before_prompt_build(&self, prompt: String) -> HookResult<String> {
        HookResult::Continue(prompt)
    }

    async fn before_llm_call(
        &self,
        messages: Vec<ChatMessage>,
        model: String,
    ) -> HookResult<(Vec<ChatMessage>, String)> {
        HookResult::Continue((messages, model))
    }

    async fn before_tool_call(&self, name: String, args: Value) -> HookResult<(String, Value)> {
        HookResult::Continue((name, args))
    }

    async fn on_message_received(&self, message: ChannelMessage) -> HookResult<ChannelMessage> {
        HookResult::Continue(message)
    }

    async fn on_message_sending(
        &self,
        channel: String,
        recipient: String,
        content: String,
    ) -> HookResult<(String, String, String)> {
        HookResult::Continue((channel, recipient, content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHook {
        name: String,
        priority: i32,
    }

    impl TestHook {
        fn new(name: &str, priority: i32) -> Self {
            Self {
                name: name.to_string(),
                priority,
            }
        }
    }

    #[async_trait]
    impl HookHandler for TestHook {
        fn name(&self) -> &str {
            &self.name
        }
        fn priority(&self) -> i32 {
            self.priority
        }
    }

    #[test]
    fn hook_result_is_cancel() {
        let ok: HookResult<String> = HookResult::Continue("hi".into());
        assert!(!ok.is_cancel());
        let cancel: HookResult<String> = HookResult::Cancel("blocked".into());
        assert!(cancel.is_cancel());
    }

    #[test]
    fn default_priority_is_zero() {
        struct MinimalHook;
        #[async_trait]
        impl HookHandler for MinimalHook {
            fn name(&self) -> &str {
                "minimal"
            }
        }
        assert_eq!(MinimalHook.priority(), 0);
    }

    #[tokio::test]
    async fn default_modifying_hooks_pass_through() {
        let hook = TestHook::new("test", 0);
        match hook
            .before_tool_call("shell".into(), serde_json::json!({"cmd": "ls"}))
            .await
        {
            HookResult::Continue((name, _args)) => assert_eq!(name, "shell"),
            HookResult::Cancel(_) => panic!("should not cancel"),
            HookResult::HardDeny(_) => panic!("should not hard deny"),
        }
    }

    #[test]
    fn hook_result_is_hard_deny() {
        let cont: HookResult<String> = HookResult::Continue("hi".into());
        assert!(!cont.is_hard_deny());
        assert!(!cont.is_soft_cancel());

        let cancel: HookResult<String> = HookResult::Cancel("soft".into());
        assert!(!cancel.is_hard_deny());
        assert!(cancel.is_soft_cancel());
        assert!(cancel.is_cancel());

        let hard: HookResult<String> = HookResult::HardDeny("security violation".into());
        assert!(hard.is_hard_deny());
        assert!(hard.is_cancel()); // HardDeny is also a cancellation
        assert!(!hard.is_soft_cancel());
    }

    #[test]
    fn hook_result_reason() {
        let cont: HookResult<String> = HookResult::Continue("hi".into());
        assert_eq!(cont.reason(), None);

        let cancel: HookResult<String> = HookResult::Cancel("soft block".into());
        assert_eq!(cancel.reason(), Some("soft block"));

        let hard: HookResult<String> = HookResult::HardDeny("budget exceeded".into());
        assert_eq!(hard.reason(), Some("budget exceeded"));
    }
}
