use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::policy::ToolOperation;
use zeroclaw_memory::{Memory, MemoryCategory};

/// Let the agent store memories — its own brain writes
pub struct MemoryStoreTool {
    memory: Arc<dyn Memory>,
    security: Arc<SecurityPolicy>,
}

impl MemoryStoreTool {
    pub fn new(memory: Arc<dyn Memory>, security: Arc<SecurityPolicy>) -> Self {
        Self { memory, security }
    }
}

#[async_trait]
impl Tool for MemoryStoreTool {
    fn name(&self) -> &str {
        "memory_store"
    }

    fn description(&self) -> &str {
        "Store a fact, preference, or note in long-term memory. Use category 'core' for permanent facts, 'daily' for session notes, 'conversation' for chat context, or a custom category name. For a fact involving a relationship between people, organizations, or entities (e.g. 'who works with whom', 'who previously worked at X') that may need multi-hop lookup later, prefer graph_remember instead when it's available."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Unique key for this memory (e.g. 'user_lang', 'project_stack')"
                },
                "content": {
                    "type": "string",
                    "description": "The information to remember"
                },
                "category": {
                    "type": "string",
                    "description": "Memory category: 'core' (permanent), 'daily' (session), 'conversation' (chat), or a custom category name. Defaults to 'core'."
                },
                "importance": {
                    "type": "number",
                    "minimum": 0.0,
                    "maximum": 1.0,
                    "description": "Optional 0-1 weight for how much this should be preferred when memory is recalled or trimmed. Use ~0.9 for standing rules and hard constraints, ~0.5 for ordinary facts, ~0.2 for passing notes. Omit to let the system score it."
                }
            },
            "required": ["key", "content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let key = args.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"param": "key"})),
                "memory_store: missing key parameter"
            );
            anyhow::Error::msg("Missing 'key' parameter")
        })?;

        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"param": "content"})),
                    "memory_store: missing content parameter"
                );
                anyhow::Error::msg("Missing 'content' parameter")
            })?;

        let category = match args.get("category").and_then(|v| v.as_str()) {
            Some("core") | None => MemoryCategory::Core,
            Some("daily") => MemoryCategory::Daily,
            Some("conversation") => MemoryCategory::Conversation,
            Some(other) => MemoryCategory::Custom(other.to_string()),
        };

        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "memory_store")
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(error),
            });
        }

        // Out-of-range or non-numeric values are ignored rather than rejected: a bad hint
        // must not cost the agent the memory itself.
        let importance = args
            .get("importance")
            .and_then(serde_json::Value::as_f64)
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(0.0, 1.0));

        let stored = match importance {
            Some(imp) => {
                self.memory
                    .store_with_metadata(key, content, category, None, None, Some(imp))
                    .await
            }
            None => self.memory.store(key, content, category, None).await,
        };

        match stored {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("Stored memory: {key}").into(),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Failed to store memory: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zeroclaw_config::autonomy::AutonomyLevel;
    use zeroclaw_config::policy::SecurityPolicy;
    use zeroclaw_memory::SqliteMemory;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::default())
    }

    fn test_mem() -> (TempDir, Arc<dyn Memory>) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new("test", tmp.path()).unwrap();
        (tmp, Arc::new(mem))
    }

    #[test]
    fn name_and_schema() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem, test_security());
        assert_eq!(tool.name(), "memory_store");
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["key"].is_object());
        assert!(schema["properties"]["content"].is_object());
    }

    #[tokio::test]
    async fn store_core() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());
        let result = tool
            .execute(json!({"key": "lang", "content": "Prefers Rust"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("lang"));

        let entry = mem.get("lang").await.unwrap();
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().content, "Prefers Rust");
    }

    #[tokio::test]
    async fn importance_is_stored_clamped_and_optional() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());

        for (key, arg, expect) in [
            ("rule", json!(0.9), Some(0.9)),
            ("over", json!(7.5), Some(1.0)),
            ("neg", json!(-2.0), Some(0.0)),
        ] {
            let r = tool
                .execute(json!({"key": key, "content": "x", "importance": arg}))
                .await
                .unwrap();
            assert!(r.success, "{key}: {:?}", r.error);
            let got = mem.get(key).await.unwrap().unwrap().importance;
            assert_eq!(got.map(|v| (v * 100.0).round() / 100.0), expect, "{key}");
        }

        // A non-numeric hint is ignored, and the memory is still stored.
        let r = tool
            .execute(json!({"key": "junk", "content": "x", "importance": "high"}))
            .await
            .unwrap();
        assert!(r.success);
        assert!(mem.get("junk").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn store_with_category() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());
        let result = tool
            .execute(json!({"key": "note", "content": "Fixed bug", "category": "daily"}))
            .await
            .unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn store_with_custom_category() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());
        let result = tool
            .execute(
                json!({"key": "proj_note", "content": "Uses async runtime", "category": "project"}),
            )
            .await
            .unwrap();
        assert!(result.success);

        let entry = mem.get("proj_note").await.unwrap().unwrap();
        assert_eq!(entry.content, "Uses async runtime");
        assert_eq!(entry.category, MemoryCategory::Custom("project".into()));
    }

    #[tokio::test]
    async fn store_missing_key() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem, test_security());
        let result = tool.execute(json!({"content": "no key"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn store_missing_content() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem, test_security());
        let result = tool.execute(json!({"key": "no_content"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn store_blocked_in_readonly_mode() {
        let (_tmp, mem) = test_mem();
        let readonly = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = MemoryStoreTool::new(mem.clone(), readonly);
        let result = tool
            .execute(json!({"key": "lang", "content": "Prefers Rust"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("read-only mode")
        );
        assert!(mem.get("lang").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn store_blocked_when_rate_limited() {
        let (_tmp, mem) = test_mem();
        let limited = Arc::new(SecurityPolicy {
            max_actions_per_hour: 0,
            ..SecurityPolicy::default()
        });
        let tool = MemoryStoreTool::new(mem.clone(), limited);
        let result = tool
            .execute(json!({"key": "lang", "content": "Prefers Rust"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Rate limit exceeded")
        );
        assert!(mem.get("lang").await.unwrap().is_none());
    }
}
