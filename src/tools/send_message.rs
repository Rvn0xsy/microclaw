use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use teloxide::prelude::*;
use teloxide::types::InputFile;

use super::{authorize_chat_access, schema_object, Tool, ToolResult};
use crate::channel::{deliver_and_store_bot_message, enforce_channel_policy};
use crate::claude::ToolDefinition;
use crate::config::Config;
use crate::db::{call_blocking, Database, StoredMessage};

pub struct SendMessageTool {
    bot: Bot,
    db: Arc<Database>,
    bot_username: String,
    config: Option<Config>,
    http_client: reqwest::Client,
}

impl SendMessageTool {
    pub fn new(bot: Bot, db: Arc<Database>, bot_username: String) -> Self {
        SendMessageTool {
            bot,
            db,
            bot_username,
            config: None,
            http_client: reqwest::Client::new(),
        }
    }

    pub fn new_with_config(
        bot: Bot,
        db: Arc<Database>,
        bot_username: String,
        config: Config,
    ) -> Self {
        SendMessageTool {
            bot,
            db,
            bot_username,
            config: Some(config),
            http_client: reqwest::Client::new(),
        }
    }

    async fn store_bot_message(&self, chat_id: i64, content: String) -> Result<(), String> {
        let msg = StoredMessage {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id,
            sender_name: self.bot_username.clone(),
            content,
            is_from_bot: true,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        call_blocking(self.db.clone(), move |db| db.store_message(&msg))
            .await
            .map_err(|e| format!("Failed to store sent message: {e}"))
    }

    async fn send_telegram_attachment(
        &self,
        chat_id: i64,
        file_path: PathBuf,
        caption: Option<String>,
    ) -> Result<String, String> {
        let mut req = self
            .bot
            .send_document(ChatId(chat_id), InputFile::file(file_path.clone()));
        if let Some(c) = &caption {
            req = req.caption(c.clone());
        }
        req.await
            .map_err(|e| format!("Failed to send Telegram attachment: {e}"))?;

        Ok(match caption {
            Some(c) => format!("[attachment:{}] {}", file_path.display(), c),
            None => format!("[attachment:{}]", file_path.display()),
        })
    }

    async fn send_discord_attachment(
        &self,
        chat_id: i64,
        file_path: PathBuf,
        caption: Option<String>,
    ) -> Result<String, String> {
        let cfg = self
            .config
            .as_ref()
            .ok_or_else(|| "send_message config unavailable".to_string())?;
        let token = cfg
            .discord_bot_token
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| "discord_bot_token not configured".to_string())?;

        let filename = file_path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("attachment.bin")
            .to_string();
        let bytes = tokio::fs::read(&file_path)
            .await
            .map_err(|e| format!("Failed to read attachment file: {e}"))?;

        let payload = json!({ "content": caption.clone().unwrap_or_default() });
        let form = reqwest::multipart::Form::new()
            .text("payload_json", payload.to_string())
            .part(
                "files[0]",
                reqwest::multipart::Part::bytes(bytes).file_name(filename),
            );

        let url = format!("https://discord.com/api/v10/channels/{chat_id}/messages");
        let resp = self
            .http_client
            .post(url)
            .header(reqwest::header::AUTHORIZATION, format!("Bot {token}"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("Failed to send Discord attachment: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "Failed to send Discord attachment: HTTP {status} {}",
                body.chars().take(300).collect::<String>()
            ));
        }

        Ok(match caption {
            Some(c) => format!("[attachment:{}] {}", file_path.display(), c),
            None => format!("[attachment:{}]", file_path.display()),
        })
    }

    async fn send_whatsapp_attachment(
        &self,
        chat_id: i64,
        file_path: PathBuf,
        caption: Option<String>,
    ) -> Result<String, String> {
        let cfg = self
            .config
            .as_ref()
            .ok_or_else(|| "send_message config unavailable".to_string())?;
        let access_token = cfg
            .whatsapp_access_token
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| "whatsapp_access_token not configured".to_string())?;
        let phone_number_id = cfg
            .whatsapp_phone_number_id
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| "whatsapp_phone_number_id not configured".to_string())?;

        let filename = file_path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("attachment.bin")
            .to_string();
        let bytes = tokio::fs::read(&file_path)
            .await
            .map_err(|e| format!("Failed to read attachment file: {e}"))?;

        let upload_url = format!("https://graph.facebook.com/v23.0/{phone_number_id}/media");
        let form = reqwest::multipart::Form::new()
            .text("messaging_product", "whatsapp")
            .part(
                "file",
                reqwest::multipart::Part::bytes(bytes)
                    .file_name(filename.clone())
                    .mime_str("application/octet-stream")
                    .map_err(|e| format!("Invalid attachment mime: {e}"))?,
            );
        let upload_resp = self
            .http_client
            .post(upload_url)
            .bearer_auth(access_token)
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("Failed to upload WhatsApp media: {e}"))?;
        if !upload_resp.status().is_success() {
            let status = upload_resp.status();
            let body = upload_resp.text().await.unwrap_or_default();
            return Err(format!(
                "Failed to upload WhatsApp media: HTTP {status} {}",
                body.chars().take(300).collect::<String>()
            ));
        }
        let upload_json: serde_json::Value = upload_resp
            .json()
            .await
            .map_err(|e| format!("Invalid WhatsApp media upload response: {e}"))?;
        let media_id = upload_json
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "WhatsApp media upload did not return id".to_string())?;

        let mut document = json!({ "id": media_id, "filename": filename });
        if let Some(c) = &caption {
            document["caption"] = json!(c);
        }
        let payload = json!({
            "messaging_product": "whatsapp",
            "to": chat_id.to_string(),
            "type": "document",
            "document": document,
        });
        let send_url = format!("https://graph.facebook.com/v23.0/{phone_number_id}/messages");
        let send_resp = self
            .http_client
            .post(send_url)
            .bearer_auth(access_token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("Failed to send WhatsApp attachment: {e}"))?;
        if !send_resp.status().is_success() {
            let status = send_resp.status();
            let body = send_resp.text().await.unwrap_or_default();
            return Err(format!(
                "Failed to send WhatsApp attachment: HTTP {status} {}",
                body.chars().take(300).collect::<String>()
            ));
        }

        Ok(match caption {
            Some(c) => format!("[attachment:{}] {}", file_path.display(), c),
            None => format!("[attachment:{}]", file_path.display()),
        })
    }
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "send_message".into(),
            description: "Send a message mid-conversation. Supports text for all channels, and attachments for Telegram/Discord/WhatsApp via attachment_path.".into(),
            input_schema: schema_object(
                json!({
                    "chat_id": {
                        "type": "integer",
                        "description": "The target chat ID"
                    },
                    "text": {
                        "type": "string",
                        "description": "The message text to send"
                    },
                    "attachment_path": {
                        "type": "string",
                        "description": "Optional local file path to send as an attachment"
                    },
                    "caption": {
                        "type": "string",
                        "description": "Optional caption used when sending attachment"
                    }
                }),
                &["chat_id"],
            ),
        }
    }

    async fn execute(&self, input: serde_json::Value) -> ToolResult {
        let chat_id = match input.get("chat_id").and_then(|v| v.as_i64()) {
            Some(id) => id,
            None => return ToolResult::error("Missing required parameter: chat_id".into()),
        };
        let text = input
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let attachment_path = input
            .get("attachment_path")
            .and_then(|v| v.as_str())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let caption = input
            .get("caption")
            .and_then(|v| v.as_str())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        if text.is_empty() && attachment_path.is_none() {
            return ToolResult::error("Provide text and/or attachment_path".into());
        }

        if let Err(e) = authorize_chat_access(&input, chat_id) {
            return ToolResult::error(e);
        }

        if let Err(e) = enforce_channel_policy(self.db.clone(), &input, chat_id).await {
            return ToolResult::error(e);
        }

        if let Some(path) = attachment_path {
            let chat_type =
                match call_blocking(self.db.clone(), move |db| db.get_chat_type(chat_id)).await {
                    Ok(v) => v,
                    Err(e) => return ToolResult::error(format!("Failed to read chat type: {e}")),
                };

            let file_path = PathBuf::from(&path);
            if !file_path.is_file() {
                return ToolResult::error(format!(
                    "attachment_path not found or not a file: {path}"
                ));
            }

            let used_caption = caption.or_else(|| {
                if text.is_empty() {
                    None
                } else {
                    Some(text.clone())
                }
            });

            let send_result = match chat_type.as_deref() {
                Some("telegram_private")
                | Some("telegram_group")
                | Some("telegram_supergroup")
                | Some("telegram_channel")
                | Some("private")
                | Some("group")
                | Some("supergroup")
                | Some("channel") => {
                    self.send_telegram_attachment(chat_id, file_path.clone(), used_caption.clone())
                        .await
                }
                Some("discord") => {
                    self.send_discord_attachment(chat_id, file_path.clone(), used_caption.clone())
                        .await
                }
                Some("whatsapp") => {
                    self.send_whatsapp_attachment(chat_id, file_path.clone(), used_caption.clone())
                        .await
                }
                Some("web") => Err("attachment sending is not supported for web chat".to_string()),
                Some(other) => Err(format!(
                    "attachment sending is not supported for chat type: {other}"
                )),
                None => Err("target chat not found".to_string()),
            };

            match send_result {
                Ok(content) => {
                    if let Err(e) = self.store_bot_message(chat_id, content).await {
                        return ToolResult::error(e);
                    }
                    ToolResult::success("Attachment sent successfully.".into())
                }
                Err(e) => ToolResult::error(e),
            }
        } else {
            match deliver_and_store_bot_message(
                &self.bot,
                self.db.clone(),
                &self.bot_username,
                chat_id,
                &text,
            )
            .await
            {
                Ok(_) => ToolResult::success("Message sent successfully.".into()),
                Err(e) => ToolResult::error(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_db() -> (Arc<Database>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("microclaw_sendmsg_{}", uuid::Uuid::new_v4()));
        let db = Arc::new(Database::new(dir.to_str().unwrap()).unwrap());
        (db, dir)
    }

    fn cleanup(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_send_message_permission_denied_before_network() {
        let (db, dir) = test_db();
        let tool = SendMessageTool::new(Bot::new("123456:TEST_TOKEN"), db, "bot".into());
        let result = tool
            .execute(json!({
                "chat_id": 200,
                "text": "hello",
                "__microclaw_auth": {
                    "caller_chat_id": 100,
                    "control_chat_ids": []
                }
            }))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("Permission denied"));
        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_send_message_web_target_writes_to_db() {
        let (db, dir) = test_db();
        db.upsert_chat(999, Some("web-main"), "web").unwrap();

        let tool = SendMessageTool::new(Bot::new("123456:TEST_TOKEN"), db.clone(), "bot".into());
        let result = tool
            .execute(json!({
                "chat_id": 999,
                "text": "hello web",
                "__microclaw_auth": {
                    "caller_chat_id": 999,
                    "control_chat_ids": []
                }
            }))
            .await;
        assert!(!result.is_error, "{}", result.content);

        let all = db.get_all_messages(999).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].content, "hello web");
        assert!(all[0].is_from_bot);
        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_send_message_web_caller_cross_chat_denied() {
        let (db, dir) = test_db();
        db.upsert_chat(100, Some("web-main"), "web").unwrap();
        db.upsert_chat(200, Some("tg"), "private").unwrap();

        let tool = SendMessageTool::new(Bot::new("123456:TEST_TOKEN"), db, "bot".into());
        let result = tool
            .execute(json!({
                "chat_id": 200,
                "text": "hello",
                "__microclaw_auth": {
                    "caller_chat_id": 100,
                    "control_chat_ids": [100]
                }
            }))
            .await;
        assert!(result.is_error);
        assert!(result
            .content
            .contains("web chats cannot operate on other chats"));
        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_send_message_requires_text_or_attachment() {
        let (db, dir) = test_db();
        let tool = SendMessageTool::new(Bot::new("123456:TEST_TOKEN"), db, "bot".into());
        let result = tool
            .execute(json!({
                "chat_id": 999,
                "text": "   "
            }))
            .await;
        assert!(result.is_error);
        assert!(result
            .content
            .contains("Provide text and/or attachment_path"));
        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_send_attachment_non_telegram_rejected_without_network() {
        let (db, dir) = test_db();
        db.upsert_chat(999, Some("web-main"), "web").unwrap();

        let attachment = dir.join("sample.txt");
        std::fs::write(&attachment, "hello").unwrap();

        let tool = SendMessageTool::new(Bot::new("123456:TEST_TOKEN"), db, "bot".into());
        let result = tool
            .execute(json!({
                "chat_id": 999,
                "attachment_path": attachment.to_string_lossy(),
                "caption": "test"
            }))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("not supported for web chat"));
        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_send_attachment_discord_without_config_fails_fast() {
        let (db, dir) = test_db();
        db.upsert_chat(123, Some("discord-123"), "discord").unwrap();

        let attachment = dir.join("sample.txt");
        std::fs::write(&attachment, "hello").unwrap();

        let tool = SendMessageTool::new(Bot::new("123456:TEST_TOKEN"), db, "bot".into());
        let result = tool
            .execute(json!({
                "chat_id": 123,
                "attachment_path": attachment.to_string_lossy(),
            }))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("config unavailable"));
        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_send_attachment_whatsapp_without_config_fails_fast() {
        let (db, dir) = test_db();
        db.upsert_chat(861234567890i64, Some("wa"), "whatsapp")
            .unwrap();

        let attachment = dir.join("sample.txt");
        std::fs::write(&attachment, "hello").unwrap();

        let tool = SendMessageTool::new(Bot::new("123456:TEST_TOKEN"), db, "bot".into());
        let result = tool
            .execute(json!({
                "chat_id": 861234567890i64,
                "attachment_path": attachment.to_string_lossy(),
            }))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("config unavailable"));
        cleanup(&dir);
    }
}
