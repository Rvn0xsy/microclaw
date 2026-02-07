use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::Config;
use crate::error::MicroClawError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: ImageSource },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub system: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct MessagesResponse {
    pub content: Vec<ResponseContentBlock>,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    error: ApiErrorDetail,
}

#[derive(Debug, Deserialize)]
struct ApiErrorDetail {
    message: String,
    #[serde(rename = "type")]
    error_type: String,
}

pub struct ClaudeClient {
    http: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl ClaudeClient {
    pub fn new(config: &Config) -> Self {
        ClaudeClient {
            http: reqwest::Client::new(),
            api_key: config.anthropic_api_key.clone(),
            model: config.claude_model.clone(),
            max_tokens: config.max_tokens,
        }
    }

    #[allow(dead_code)]
    pub fn model(&self) -> &str {
        &self.model
    }

    pub async fn send_message(
        &self,
        system: &str,
        messages: Vec<Message>,
        tools: Option<Vec<ToolDefinition>>,
    ) -> Result<MessagesResponse, MicroClawError> {
        let request = MessagesRequest {
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            system: system.to_string(),
            messages,
            tools,
        };

        let mut retries = 0;
        let max_retries = 3;

        loop {
            let response = self
                .http
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&request)
                .send()
                .await?;

            let status = response.status();

            if status.is_success() {
                let body = response.text().await?;
                let parsed: MessagesResponse = serde_json::from_str(&body).map_err(|e| {
                    MicroClawError::AnthropicApi(format!(
                        "Failed to parse response: {e}\nBody: {body}"
                    ))
                })?;
                return Ok(parsed);
            }

            if status.as_u16() == 429 && retries < max_retries {
                retries += 1;
                let delay = std::time::Duration::from_secs(2u64.pow(retries));
                warn!("Rate limited, retrying in {:?} (attempt {retries}/{max_retries})", delay);
                tokio::time::sleep(delay).await;
                continue;
            }

            let body = response.text().await.unwrap_or_default();
            if let Ok(api_err) = serde_json::from_str::<ApiError>(&body) {
                return Err(MicroClawError::AnthropicApi(format!(
                    "{}: {}",
                    api_err.error.error_type, api_err.error.message
                )));
            }
            return Err(MicroClawError::AnthropicApi(format!(
                "HTTP {status}: {body}"
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_content_block_text_serialization() {
        let block = ContentBlock::Text {
            text: "hello".into(),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hello");
    }

    #[test]
    fn test_content_block_tool_use_serialization() {
        let block = ContentBlock::ToolUse {
            id: "id_123".into(),
            name: "bash".into(),
            input: json!({"command": "ls"}),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_use");
        assert_eq!(json["id"], "id_123");
        assert_eq!(json["name"], "bash");
        assert_eq!(json["input"]["command"], "ls");
    }

    #[test]
    fn test_content_block_tool_result_serialization() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "id_123".into(),
            content: "output".into(),
            is_error: Some(true),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["tool_use_id"], "id_123");
        assert_eq!(json["is_error"], true);
    }

    #[test]
    fn test_content_block_tool_result_skips_none_is_error() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "id_123".into(),
            content: "output".into(),
            is_error: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert!(json.get("is_error").is_none());
    }

    #[test]
    fn test_message_content_text_serialization() {
        let msg = Message {
            role: "user".into(),
            content: MessageContent::Text("hello".into()),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn test_message_content_blocks_serialization() {
        let msg = Message {
            role: "assistant".into(),
            content: MessageContent::Blocks(vec![ContentBlock::Text {
                text: "thinking...".into(),
            }]),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "assistant");
        assert!(json["content"].is_array());
        assert_eq!(json["content"][0]["type"], "text");
    }

    #[test]
    fn test_messages_response_deserialization() {
        let json = json!({
            "content": [
                {"type": "text", "text": "Hello!"}
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5
            }
        });
        let resp: MessagesResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            ResponseContentBlock::Text { text } => assert_eq!(text, "Hello!"),
            _ => panic!("Expected Text block"),
        }
        assert_eq!(resp.usage.as_ref().unwrap().input_tokens, 10);
        assert_eq!(resp.usage.as_ref().unwrap().output_tokens, 5);
    }

    #[test]
    fn test_response_content_block_tool_use_deserialization() {
        let json = json!({
            "type": "tool_use",
            "id": "tu_abc",
            "name": "bash",
            "input": {"command": "echo hi"}
        });
        let block: ResponseContentBlock = serde_json::from_value(json).unwrap();
        match block {
            ResponseContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "tu_abc");
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "echo hi");
            }
            _ => panic!("Expected ToolUse block"),
        }
    }

    #[test]
    fn test_messages_request_serialization() {
        let req = MessagesRequest {
            model: "claude-sonnet-4-20250514".into(),
            max_tokens: 4096,
            system: "You are helpful.".into(),
            messages: vec![Message {
                role: "user".into(),
                content: MessageContent::Text("hi".into()),
            }],
            tools: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "claude-sonnet-4-20250514");
        assert_eq!(json["max_tokens"], 4096);
        assert!(json.get("tools").is_none()); // skip_serializing_if None
    }

    #[test]
    fn test_messages_request_with_tools() {
        let req = MessagesRequest {
            model: "test".into(),
            max_tokens: 100,
            system: "sys".into(),
            messages: vec![],
            tools: Some(vec![ToolDefinition {
                name: "bash".into(),
                description: "Run bash".into(),
                input_schema: json!({"type": "object"}),
            }]),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json["tools"].is_array());
        assert_eq!(json["tools"][0]["name"], "bash");
    }

    #[test]
    fn test_claude_client_new() {
        let config = crate::config::Config {
            telegram_bot_token: "tok".into(),
            anthropic_api_key: "key123".into(),
            bot_username: "bot".into(),
            claude_model: "claude-test".into(),
            data_dir: "/tmp".into(),
            max_tokens: 2048,
            max_tool_iterations: 10,
            max_history_messages: 20,
            openai_api_key: None,
            timezone: "UTC".into(),
            allowed_groups: vec![],
            max_session_messages: 40,
            compact_keep_recent: 20,
            whatsapp_access_token: None,
            whatsapp_phone_number_id: None,
            whatsapp_verify_token: None,
            whatsapp_webhook_port: 8080,
        };
        let client = ClaudeClient::new(&config);
        assert_eq!(client.model(), "claude-test");
        assert_eq!(client.max_tokens, 2048);
    }

    #[test]
    fn test_content_block_image_serialization() {
        let block = ContentBlock::Image {
            source: ImageSource {
                source_type: "base64".into(),
                media_type: "image/jpeg".into(),
                data: "abc123".into(),
            },
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "image");
        assert_eq!(json["source"]["type"], "base64");
        assert_eq!(json["source"]["media_type"], "image/jpeg");
        assert_eq!(json["source"]["data"], "abc123");
    }

    #[test]
    fn test_image_source_serialization() {
        let source = ImageSource {
            source_type: "base64".into(),
            media_type: "image/png".into(),
            data: "ABCDEF".into(),
        };
        let json = serde_json::to_value(&source).unwrap();
        assert_eq!(json["type"], "base64");
        assert_eq!(json["media_type"], "image/png");
    }
}
