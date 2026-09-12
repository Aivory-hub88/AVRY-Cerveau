use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_memory::Memory;

/// Let the agent search its own past conversation turns verbatim, as
/// opposed to `memory_recall`'s curated facts/preferences or
/// `graph_recall`'s relationship queries. See `Memory::recall_conversation`
/// for why this is a separate method/tool rather than a `memory_recall`
/// flag: raw turns and curated memories serve different intents, and
/// mixing them would either bury one under the other's ranking or force a
/// filter parameter onto every existing `memory_recall` caller.
pub struct SessionSearchTool {
    memory: Arc<dyn Memory>,
}

impl SessionSearchTool {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        "Search your own past conversation turns verbatim -- exact phrases and figures as they \
         were actually said, not a summarized or curated fact. Use this when you need to recall \
         precisely what was said (an exact quote, an invoice number, a wording someone used), \
         not a gist. For curated long-term facts/preferences, use memory_recall instead; for \
         relationship/multi-hop questions, use graph_recall."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords or phrase to search for verbatim in past conversation turns. Omit or pass bare '*' to return recent turns."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (default: 5)"
                },
                "since": {
                    "type": "string",
                    "description": "Filter turns at or after this time (RFC 3339, e.g. 2025-03-01T00:00:00Z)"
                },
                "until": {
                    "type": "string",
                    "description": "Filter turns at or before this time (RFC 3339)"
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let since = args.get("since").and_then(|v| v.as_str());
        let until = args.get("until").and_then(|v| v.as_str());

        if let Some(s) = since
            && chrono::DateTime::parse_from_rfc3339(s).is_err()
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Invalid 'since' date: {s}. Expected RFC 3339 format, e.g. 2025-03-01T00:00:00Z"
                )),
            });
        }
        if let Some(u) = until
            && chrono::DateTime::parse_from_rfc3339(u).is_err()
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Invalid 'until' date: {u}. Expected RFC 3339 format, e.g. 2025-03-01T00:00:00Z"
                )),
            });
        }
        if let (Some(s), Some(u)) = (since, until)
            && let (Ok(s_dt), Ok(u_dt)) = (
                chrono::DateTime::parse_from_rfc3339(s),
                chrono::DateTime::parse_from_rfc3339(u),
            )
            && s_dt >= u_dt
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("'since' must be before 'until'".into()),
            });
        }

        #[allow(clippy::cast_possible_truncation)]
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(5, |v| v as usize);

        match self.memory.recall_conversation(query, limit, None, since, until).await {
            Ok(entries) if entries.is_empty() => Ok(ToolResult {
                success: true,
                output: "No past conversation turns found.".into(),
                error: None,
            }),
            Ok(entries) => {
                let mut output = format!("Found {} past turn(s):\n", entries.len());
                for entry in &entries {
                    let score = entry
                        .score
                        .map_or_else(String::new, |s| format!(" [{:.0}%]", s * 100.0));
                    let _ = writeln!(output, "- [{}] {}{score}", entry.timestamp, entry.content);
                }
                Ok(ToolResult {
                    success: true,
                    output: output.into(),
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Session search failed: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zeroclaw_memory::{MemoryCategory, SqliteMemory};

    fn seeded_mem() -> (TempDir, Arc<dyn Memory>) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new("test", tmp.path()).unwrap();
        (tmp, Arc::new(mem))
    }

    #[test]
    fn name_and_schema() {
        let (_tmp, mem) = seeded_mem();
        let tool = SessionSearchTool::new(mem);
        assert_eq!(tool.name(), "session_search");
        assert!(tool.parameters_schema()["properties"]["query"].is_object());
    }

    #[tokio::test]
    async fn search_empty() {
        let (_tmp, mem) = seeded_mem();
        let tool = SessionSearchTool::new(mem);
        let result = tool.execute(json!({"query": "anything"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No past conversation turns found"));
    }

    #[tokio::test]
    async fn search_finds_only_conversation_category() {
        let (_tmp, mem) = seeded_mem();
        mem.store(
            "user_msg_1",
            "The invoice number is INV-48291-B",
            MemoryCategory::Conversation,
            None,
        )
        .await
        .unwrap();
        mem.store(
            "core_fact",
            "User prefers invoice-related updates by email",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

        let tool = SessionSearchTool::new(mem);
        let result = tool.execute(json!({"query": "invoice"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("INV-48291-B"));
        assert!(!result.output.contains("prefers invoice-related updates"));
    }

    #[tokio::test]
    async fn search_invalid_since_date() {
        let (_tmp, mem) = seeded_mem();
        let tool = SessionSearchTool::new(mem);
        let result = tool
            .execute(json!({"query": "x", "since": "not-a-date"}))
            .await
            .unwrap();
        assert!(!result.success);
    }
}
