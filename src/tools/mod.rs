pub mod activate_skill;
pub mod bash;
pub mod edit_file;
pub mod export_chat;
pub mod glob;
pub mod grep;
pub mod mcp;
pub mod memory;
pub mod read_file;
pub mod schedule;
pub mod send_message;
pub mod sub_agent;
pub mod todo;
pub mod web_fetch;
pub mod web_search;
pub mod write_file;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use teloxide::prelude::*;

use crate::claude::ToolDefinition;
use crate::config::Config;
use crate::db::Database;

pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn success(content: String) -> Self {
        ToolResult {
            content,
            is_error: false,
        }
    }

    pub fn error(content: String) -> Self {
        ToolResult {
            content,
            is_error: true,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, input: serde_json::Value) -> ToolResult;
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new(config: &Config, bot: Bot, db: Arc<Database>) -> Self {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(bash::BashTool),
            Box::new(read_file::ReadFileTool),
            Box::new(write_file::WriteFileTool),
            Box::new(edit_file::EditFileTool),
            Box::new(glob::GlobTool),
            Box::new(grep::GrepTool),
            Box::new(memory::ReadMemoryTool::new(&config.data_dir)),
            Box::new(memory::WriteMemoryTool::new(&config.data_dir)),
            Box::new(web_fetch::WebFetchTool),
            Box::new(web_search::WebSearchTool),
            Box::new(send_message::SendMessageTool::new(bot)),
            Box::new(schedule::ScheduleTaskTool::new(db.clone(), config.timezone.clone())),
            Box::new(schedule::ListTasksTool::new(db.clone())),
            Box::new(schedule::PauseTaskTool::new(db.clone())),
            Box::new(schedule::ResumeTaskTool::new(db.clone())),
            Box::new(schedule::CancelTaskTool::new(db.clone())),
            Box::new(schedule::GetTaskHistoryTool::new(db.clone())),
            Box::new(export_chat::ExportChatTool::new(db, &config.data_dir)),
            Box::new(sub_agent::SubAgentTool::new(config)),
            Box::new(activate_skill::ActivateSkillTool::new(&config.data_dir)),
            Box::new(todo::TodoReadTool::new(&config.data_dir)),
            Box::new(todo::TodoWriteTool::new(&config.data_dir)),
        ];
        ToolRegistry { tools }
    }

    /// Create a restricted tool registry for sub-agents (no side-effect or recursive tools).
    pub fn new_sub_agent(config: &Config) -> Self {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(bash::BashTool),
            Box::new(read_file::ReadFileTool),
            Box::new(write_file::WriteFileTool),
            Box::new(edit_file::EditFileTool),
            Box::new(glob::GlobTool),
            Box::new(grep::GrepTool),
            Box::new(memory::ReadMemoryTool::new(&config.data_dir)),
            Box::new(web_fetch::WebFetchTool),
            Box::new(web_search::WebSearchTool),
            Box::new(activate_skill::ActivateSkillTool::new(&config.data_dir)),
        ];
        ToolRegistry { tools }
    }

    pub fn add_tool(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }

    pub async fn execute(&self, name: &str, input: serde_json::Value) -> ToolResult {
        for tool in &self.tools {
            if tool.name() == name {
                return tool.execute(input).await;
            }
        }
        ToolResult::error(format!("Unknown tool: {name}"))
    }
}

/// Helper to build a JSON Schema object with required properties.
pub fn schema_object(
    properties: serde_json::Value,
    required: &[&str],
) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_result_success() {
        let r = ToolResult::success("ok".into());
        assert_eq!(r.content, "ok");
        assert!(!r.is_error);
    }

    #[test]
    fn test_tool_result_error() {
        let r = ToolResult::error("fail".into());
        assert_eq!(r.content, "fail");
        assert!(r.is_error);
    }

    #[test]
    fn test_schema_object() {
        let schema = schema_object(
            json!({
                "name": {"type": "string"},
                "age": {"type": "integer"}
            }),
            &["name"],
        );
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["name"].is_object());
        assert!(schema["properties"]["age"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "name");
    }

    #[test]
    fn test_schema_object_empty_required() {
        let schema = schema_object(json!({}), &[]);
        let required = schema["required"].as_array().unwrap();
        assert!(required.is_empty());
    }
}
