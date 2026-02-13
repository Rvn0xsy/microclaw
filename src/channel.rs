use std::sync::Arc;

use teloxide::prelude::*;

use crate::channels::delivery::{send_discord_text, send_telegram_text};
use crate::config::Config;
use crate::db::{call_blocking, Database, StoredMessage};
use crate::tools::auth_context_from_input;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatChannel {
    Telegram,
    Discord,
    Web,
}

impl ChatChannel {
    pub fn as_caller_channel(self) -> &'static str {
        match self {
            ChatChannel::Telegram => "telegram",
            ChatChannel::Discord => "discord",
            ChatChannel::Web => "web",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationKind {
    Private,
    Group,
}

impl ConversationKind {
    pub fn as_agent_chat_type(self) -> &'static str {
        match self {
            ConversationKind::Private => "private",
            ConversationKind::Group => "group",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatRouting {
    pub channel: ChatChannel,
    pub conversation: ConversationKind,
}

pub fn parse_chat_routing(db_chat_type: &str) -> Option<ChatRouting> {
    let routing = match db_chat_type {
        "telegram_private" | "private" => ChatRouting {
            channel: ChatChannel::Telegram,
            conversation: ConversationKind::Private,
        },
        "telegram_group"
        | "telegram_supergroup"
        | "telegram_channel"
        | "group"
        | "supergroup"
        | "channel" => ChatRouting {
            channel: ChatChannel::Telegram,
            conversation: ConversationKind::Group,
        },
        "discord" => ChatRouting {
            channel: ChatChannel::Discord,
            conversation: ConversationKind::Private,
        },
        "web" => ChatRouting {
            channel: ChatChannel::Web,
            conversation: ConversationKind::Private,
        },
        _ => return None,
    };
    Some(routing)
}

pub async fn get_chat_type_raw(db: Arc<Database>, chat_id: i64) -> Result<Option<String>, String> {
    call_blocking(db, move |d| d.get_chat_type(chat_id))
        .await
        .map_err(|e| format!("Failed to read chat type for chat {chat_id}: {e}"))
}

pub async fn get_chat_routing(
    db: Arc<Database>,
    chat_id: i64,
) -> Result<Option<ChatRouting>, String> {
    let chat_type = get_chat_type_raw(db, chat_id).await?;
    Ok(chat_type.as_deref().and_then(parse_chat_routing))
}

pub async fn get_required_chat_routing(
    db: Arc<Database>,
    chat_id: i64,
) -> Result<ChatRouting, String> {
    let chat_type = get_chat_type_raw(db, chat_id)
        .await?
        .ok_or_else(|| format!("target chat {chat_id} not found"))?;
    parse_chat_routing(&chat_type)
        .ok_or_else(|| format!("unsupported chat type '{chat_type}' for chat {chat_id}"))
}

pub fn session_source_for_chat(chat_type: &str, chat_title: Option<&str>) -> String {
    if matches!(chat_type, "private" | "group" | "supergroup" | "channel")
        && chat_title.is_some_and(|t| t.starts_with("discord-"))
    {
        return "discord".to_string();
    }

    if let Some(routing) = parse_chat_routing(chat_type) {
        return match routing.channel {
            ChatChannel::Web => "web".to_string(),
            ChatChannel::Discord => "discord".to_string(),
            ChatChannel::Telegram => "telegram".to_string(),
        };
    }

    chat_type.to_string()
}

pub async fn is_web_chat(db: Arc<Database>, chat_id: i64) -> bool {
    get_chat_routing(db, chat_id)
        .await
        .ok()
        .flatten()
        .map(|r| r.channel == ChatChannel::Web)
        .unwrap_or(false)
}

pub async fn enforce_channel_policy(
    db: Arc<Database>,
    input: &serde_json::Value,
    target_chat_id: i64,
) -> Result<(), String> {
    let Some(auth) = auth_context_from_input(input) else {
        return Ok(());
    };

    if is_web_chat(db, auth.caller_chat_id).await && auth.caller_chat_id != target_chat_id {
        return Err("Permission denied: web chats cannot operate on other chats".into());
    }

    Ok(())
}

pub async fn deliver_and_store_bot_message(
    telegram_bot: Option<&Bot>,
    config: Option<&Config>,
    db: Arc<Database>,
    bot_username: &str,
    chat_id: i64,
    text: &str,
) -> Result<(), String> {
    let routing = get_required_chat_routing(db.clone(), chat_id).await?;

    match routing.channel {
        ChatChannel::Web => {}
        ChatChannel::Telegram => {
            let bot = telegram_bot.ok_or_else(|| {
                "telegram_bot_token not configured for Telegram delivery".to_string()
            })?;
            send_telegram_text(bot, chat_id, text).await?;
        }
        ChatChannel::Discord => {
            let cfg = config.ok_or_else(|| "send_message config unavailable".to_string())?;
            send_discord_text(cfg, chat_id, text).await?;
        }
    }

    let msg = StoredMessage {
        id: uuid::Uuid::new_v4().to_string(),
        chat_id,
        sender_name: bot_username.to_string(),
        content: text.to_string(),
        is_from_bot: true,
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    call_blocking(db.clone(), move |d| d.store_message(&msg))
        .await
        .map_err(|e| format!("Failed to store sent message: {e}"))
}
