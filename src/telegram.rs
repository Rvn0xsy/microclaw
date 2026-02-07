use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::ChatAction;
use tracing::{error, info};

use crate::claude::{
    ClaudeClient, ContentBlock, ImageSource, Message, MessageContent, ResponseContentBlock,
};
use crate::config::Config;
use crate::db::{Database, StoredMessage};
use crate::memory::MemoryManager;
use crate::skills::SkillManager;
use crate::tools::ToolRegistry;

pub(crate) struct AppState {
    pub config: Config,
    pub bot: Bot,
    pub db: Arc<Database>,
    pub memory: MemoryManager,
    pub skills: SkillManager,
    pub claude: ClaudeClient,
    pub tools: ToolRegistry,
}

pub async fn run_bot(
    config: Config,
    db: Database,
    memory: MemoryManager,
    skills: SkillManager,
    mcp_manager: crate::mcp::McpManager,
) -> anyhow::Result<()> {
    let bot = Bot::new(&config.telegram_bot_token);
    let db = Arc::new(db);

    let claude = ClaudeClient::new(&config);
    let mut tools = ToolRegistry::new(&config, bot.clone(), db.clone());

    // Register MCP tools
    for (server, tool_info) in mcp_manager.all_tools() {
        tools.add_tool(Box::new(crate::tools::mcp::McpTool::new(server, tool_info)));
    }

    let state = Arc::new(AppState {
        config,
        bot: bot.clone(),
        db,
        memory,
        skills,
        claude,
        tools,
    });

    // Start scheduler
    crate::scheduler::spawn_scheduler(state.clone());

    // Start WhatsApp webhook server if configured
    if let (Some(token), Some(phone_id), Some(verify)) = (
        &state.config.whatsapp_access_token,
        &state.config.whatsapp_phone_number_id,
        &state.config.whatsapp_verify_token,
    ) {
        let wa_state = state.clone();
        let token = token.clone();
        let phone_id = phone_id.clone();
        let verify = verify.clone();
        let port = state.config.whatsapp_webhook_port;
        info!("Starting WhatsApp webhook server on port {port}");
        tokio::spawn(async move {
            crate::whatsapp::start_whatsapp_server(wa_state, token, phone_id, verify, port).await;
        });
    }

    let handler = Update::filter_message().endpoint(handle_message);

    Dispatcher::builder(bot, handler)
        .default_handler(|_| async {})
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

async fn handle_message(
    bot: Bot,
    msg: teloxide::types::Message,
    state: Arc<AppState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Extract content: text, photo, or voice
    let mut text = msg.text().unwrap_or("").to_string();
    let mut image_data: Option<(String, String)> = None; // (base64, media_type)

    // Handle /reset command — clear session
    if text.trim() == "/reset" {
        let chat_id = msg.chat.id.0;
        let _ = state.db.delete_session(chat_id);
        let _ = bot
            .send_message(msg.chat.id, "Session cleared.")
            .await;
        return Ok(());
    }

    // Handle /skills command — list available skills
    if text.trim() == "/skills" {
        let formatted = state.skills.list_skills_formatted();
        let _ = bot.send_message(msg.chat.id, formatted).await;
        return Ok(());
    }

    if let Some(photos) = msg.photo() {
        // Pick the largest photo (last in the array)
        if let Some(photo) = photos.last() {
            match download_telegram_file(&bot, &photo.file.id.0).await {
                Ok(bytes) => {
                    let base64 = base64_encode(&bytes);
                    let media_type = guess_image_media_type(&bytes);
                    image_data = Some((base64, media_type));
                }
                Err(e) => {
                    error!("Failed to download photo: {e}");
                }
            }
        }
        // Use caption as text if present
        if text.is_empty() {
            text = msg.caption().unwrap_or("").to_string();
        }
    }

    // Handle voice messages
    if let Some(voice) = msg.voice() {
        if let Some(ref openai_key) = state.config.openai_api_key {
            match download_telegram_file(&bot, &voice.file.id.0).await {
                Ok(bytes) => {
                    let sender_name = msg
                        .from
                        .as_ref()
                        .map(|u| u.username.clone().unwrap_or_else(|| u.first_name.clone()))
                        .unwrap_or_else(|| "Unknown".into());
                    match crate::transcribe::transcribe_audio(openai_key, &bytes).await {
                        Ok(transcription) => {
                            text = format!("[voice message from {sender_name}]: {transcription}");
                        }
                        Err(e) => {
                            error!("Whisper transcription failed: {e}");
                            text = format!("[voice message from {sender_name}]: [transcription failed: {e}]");
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to download voice message: {e}");
                }
            }
        } else {
            let _ = bot
                .send_message(
                    msg.chat.id,
                    "Voice messages not supported (no Whisper API key configured)",
                )
                .await;
            return Ok(());
        }
    }

    // If no text and no image, nothing to process
    if text.is_empty() && image_data.is_none() {
        return Ok(());
    }

    let chat_id = msg.chat.id.0;
    let sender_name = msg
        .from
        .as_ref()
        .map(|u| {
            u.username
                .clone()
                .unwrap_or_else(|| u.first_name.clone())
        })
        .unwrap_or_else(|| "Unknown".into());

    let chat_type = match msg.chat.kind {
        teloxide::types::ChatKind::Private(_) => "private",
        _ => "group",
    };

    let chat_title = msg.chat.title().map(|t| t.to_string());

    // Check group allowlist
    if chat_type == "group" && !state.config.allowed_groups.is_empty() {
        if !state.config.allowed_groups.contains(&chat_id) {
            // Store message but don't process
            let _ = state
                .db
                .upsert_chat(chat_id, chat_title.as_deref(), chat_type);
            let stored_content = if image_data.is_some() {
                format!("[image]{}", if text.is_empty() { String::new() } else { format!(" {text}") })
            } else {
                text
            };
            let stored = StoredMessage {
                id: msg.id.0.to_string(),
                chat_id,
                sender_name,
                content: stored_content,
                is_from_bot: false,
                timestamp: chrono::Utc::now().to_rfc3339(),
            };
            let _ = state.db.store_message(&stored);
            return Ok(());
        }
    }

    // Store the chat and message
    let _ = state
        .db
        .upsert_chat(chat_id, chat_title.as_deref(), chat_type);

    let stored_content = if image_data.is_some() {
        format!("[image]{}", if text.is_empty() { String::new() } else { format!(" {text}") })
    } else {
        text.clone()
    };
    let stored = StoredMessage {
        id: msg.id.0.to_string(),
        chat_id,
        sender_name: sender_name.clone(),
        content: stored_content,
        is_from_bot: false,
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    let _ = state.db.store_message(&stored);

    // Determine if we should respond
    let should_respond = match chat_type {
        "private" => true,
        _ => {
            let bot_mention = format!("@{}", state.config.bot_username);
            text.contains(&bot_mention)
        }
    };

    if !should_respond {
        return Ok(());
    }

    info!(
        "Processing message from {} in chat {}: {}",
        sender_name,
        chat_id,
        text.chars().take(100).collect::<String>()
    );

    // Start continuous typing indicator
    let typing_chat_id = msg.chat.id;
    let typing_bot = bot.clone();
    let typing_handle = tokio::spawn(async move {
        loop {
            let _ = typing_bot
                .send_chat_action(typing_chat_id, ChatAction::Typing)
                .await;
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        }
    });

    // Process with Claude
    match process_with_claude(&state, chat_id, &sender_name, chat_type, None, image_data).await {
        Ok(response) => {
            typing_handle.abort();

            if !response.is_empty() {
                send_response(&bot, msg.chat.id, &response).await;

                // Store bot response
                let bot_msg = StoredMessage {
                    id: uuid::Uuid::new_v4().to_string(),
                    chat_id,
                    sender_name: state.config.bot_username.clone(),
                    content: response,
                    is_from_bot: true,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                };
                let _ = state.db.store_message(&bot_msg);
            }
            // If response is empty, agent likely used send_message tool directly
        }
        Err(e) => {
            typing_handle.abort();
            error!("Error processing message: {}", e);
            let _ = bot
                .send_message(msg.chat.id, format!("Error: {e}"))
                .await;
        }
    }

    Ok(())
}

async fn download_telegram_file(
    bot: &Bot,
    file_id: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let file = bot.get_file(teloxide::types::FileId(file_id.to_string())).await?;
    let mut buf = Vec::new();
    teloxide::net::Download::download_file(bot, &file.path, &mut buf).await?;
    Ok(buf)
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn guess_image_media_type(data: &[u8]) -> String {
    if data.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        "image/png".into()
    } else if data.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg".into()
    } else if data.starts_with(b"GIF") {
        "image/gif".into()
    } else if data.starts_with(b"RIFF") && data.len() >= 12 && &data[8..12] == b"WEBP" {
        "image/webp".into()
    } else {
        "image/jpeg".into() // default
    }
}

pub(crate) async fn process_with_claude(
    state: &AppState,
    chat_id: i64,
    _sender_name: &str,
    chat_type: &str,
    override_prompt: Option<&str>,
    image_data: Option<(String, String)>,
) -> anyhow::Result<String> {
    // Build system prompt
    let memory_context = state.memory.build_memory_context(chat_id);
    let skills_catalog = state.skills.build_skills_catalog();
    let system_prompt = build_system_prompt(&state.config.bot_username, &memory_context, chat_id, &skills_catalog);

    // Try to resume from session
    let mut messages = if let Some((json, updated_at)) = state.db.load_session(chat_id)? {
        // Session exists — deserialize and append new user messages
        let mut session_messages: Vec<Message> =
            serde_json::from_str(&json).unwrap_or_default();

        if session_messages.is_empty() {
            // Corrupted session, fall back to DB history
            load_messages_from_db(state, chat_id, chat_type)?
        } else {
            // Get new user messages since session was last saved
            let new_msgs = state.db.get_new_user_messages_since(chat_id, &updated_at)?;
            for stored_msg in &new_msgs {
                let content = format!("[{}]: {}", stored_msg.sender_name, stored_msg.content);
                // Merge if last message is also from user
                if let Some(last) = session_messages.last_mut() {
                    if last.role == "user" {
                        if let MessageContent::Text(t) = &mut last.content {
                            t.push('\n');
                            t.push_str(&content);
                            continue;
                        }
                    }
                }
                session_messages.push(Message {
                    role: "user".into(),
                    content: MessageContent::Text(content),
                });
            }
            session_messages
        }
    } else {
        // No session — build from DB history
        load_messages_from_db(state, chat_id, chat_type)?
    };

    // If override_prompt is provided (from scheduler), add it as a user message
    if let Some(prompt) = override_prompt {
        messages.push(Message {
            role: "user".into(),
            content: MessageContent::Text(format!("[scheduler]: {prompt}")),
        });
    }

    // If image_data is present, convert the last user message to a blocks-based message with the image
    if let Some((base64_data, media_type)) = image_data {
        if let Some(last_msg) = messages.last_mut() {
            if last_msg.role == "user" {
                let text_content = match &last_msg.content {
                    MessageContent::Text(t) => t.clone(),
                    _ => String::new(),
                };
                let mut blocks = vec![ContentBlock::Image {
                    source: ImageSource {
                        source_type: "base64".into(),
                        media_type,
                        data: base64_data,
                    },
                }];
                if !text_content.is_empty() {
                    blocks.push(ContentBlock::Text {
                        text: text_content,
                    });
                }
                last_msg.content = MessageContent::Blocks(blocks);
            }
        }
    }

    // Ensure we have at least one message
    if messages.is_empty() {
        return Ok("I didn't receive any message to process.".into());
    }

    // Compact if messages exceed threshold
    if messages.len() > state.config.max_session_messages {
        messages = compact_messages(
            &state.claude,
            &messages,
            state.config.compact_keep_recent,
        )
        .await;
    }

    let tool_defs = state.tools.definitions();

    // Agentic tool-use loop
    for iteration in 0..state.config.max_tool_iterations {
        let response = state
            .claude
            .send_message(&system_prompt, messages.clone(), Some(tool_defs.clone()))
            .await?;

        let stop_reason = response.stop_reason.as_deref().unwrap_or("end_turn");

        if stop_reason == "end_turn" || stop_reason == "max_tokens" {
            let text = response
                .content
                .iter()
                .filter_map(|block| match block {
                    ResponseContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");

            // Add final assistant message and save session
            messages.push(Message {
                role: "assistant".into(),
                content: MessageContent::Text(text.clone()),
            });
            strip_images_for_session(&mut messages);
            if let Ok(json) = serde_json::to_string(&messages) {
                let _ = state.db.save_session(chat_id, &json);
            }

            return Ok(text);
        }

        if stop_reason == "tool_use" {
            let assistant_content: Vec<ContentBlock> = response
                .content
                .iter()
                .map(|block| match block {
                    ResponseContentBlock::Text { text } => ContentBlock::Text {
                        text: text.clone(),
                    },
                    ResponseContentBlock::ToolUse { id, name, input } => {
                        ContentBlock::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        }
                    }
                })
                .collect();

            messages.push(Message {
                role: "assistant".into(),
                content: MessageContent::Blocks(assistant_content),
            });

            let mut tool_results = Vec::new();
            for block in &response.content {
                if let ResponseContentBlock::ToolUse { id, name, input } = block {
                    info!("Executing tool: {} (iteration {})", name, iteration + 1);
                    let result = state.tools.execute(name, input.clone()).await;
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: result.content,
                        is_error: if result.is_error { Some(true) } else { None },
                    });
                }
            }

            messages.push(Message {
                role: "user".into(),
                content: MessageContent::Blocks(tool_results),
            });

            continue;
        }

        // Unknown stop reason
        let text = response
            .content
            .iter()
            .filter_map(|block| match block {
                ResponseContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        // Save session even on unknown stop reason
        messages.push(Message {
            role: "assistant".into(),
            content: MessageContent::Text(text.clone()),
        });
        strip_images_for_session(&mut messages);
        if let Ok(json) = serde_json::to_string(&messages) {
            let _ = state.db.save_session(chat_id, &json);
        }

        return Ok(if text.is_empty() {
            "(no response)".into()
        } else {
            text
        });
    }

    // Save session even at max iterations
    strip_images_for_session(&mut messages);
    if let Ok(json) = serde_json::to_string(&messages) {
        let _ = state.db.save_session(chat_id, &json);
    }

    Ok("I reached the maximum number of tool iterations. Here's what I was working on — please try breaking your request into smaller steps.".into())
}

/// Load messages from DB history (non-session path).
fn load_messages_from_db(
    state: &AppState,
    chat_id: i64,
    chat_type: &str,
) -> Result<Vec<Message>, anyhow::Error> {
    let history = if chat_type == "group" {
        state.db.get_messages_since_last_bot_response(
            chat_id,
            state.config.max_history_messages,
            state.config.max_history_messages,
        )?
    } else {
        state
            .db
            .get_recent_messages(chat_id, state.config.max_history_messages)?
    };
    Ok(history_to_claude_messages(&history, &state.config.bot_username))
}

fn build_system_prompt(bot_username: &str, memory_context: &str, chat_id: i64, skills_catalog: &str) -> String {
    let mut prompt = format!(
        r#"You are {bot_username}, a helpful AI assistant on Telegram. You can execute tools to help users with tasks.

You have access to the following capabilities:
- Execute bash commands
- Read, write, and edit files
- Search for files using glob patterns
- Search file contents using regex
- Read and write persistent memory
- Search the web (web_search) and fetch web pages (web_fetch)
- Send messages mid-conversation (send_message) — use this to send intermediate updates
- Schedule tasks (schedule_task, list_scheduled_tasks, pause/resume/cancel_scheduled_task, get_task_history)
- Export chat history to markdown (export_chat)
- Understand images sent by users (they appear as image content blocks)
- Delegate self-contained sub-tasks to a parallel agent (sub_agent)
- Activate agent skills (activate_skill) for specialized tasks
- Plan and track tasks with a todo list (todo_read, todo_write) — use this to break down complex tasks into steps, track progress, and stay organized

The current chat_id is {chat_id}. Use this when calling send_message, schedule, or todo tools.

For complex, multi-step tasks: use todo_write to create a plan first, then execute each step and update the todo list as you go. This helps you stay organized and lets the user see progress.

When using memory tools, use 'chat' scope for chat-specific memories and 'global' scope for information relevant across all chats.

For scheduling:
- Use 6-field cron format: sec min hour dom month dow (e.g., "0 */5 * * * *" for every 5 minutes)
- For standard 5-field cron from the user, prepend "0 " to add the seconds field
- Use schedule_type "once" with an ISO 8601 timestamp for one-time tasks

Be concise and helpful. When executing commands or tools, show the relevant results to the user.
"#
    );

    if !memory_context.is_empty() {
        prompt.push_str("\n# Memories\n\n");
        prompt.push_str(memory_context);
    }

    if !skills_catalog.is_empty() {
        prompt.push_str("\n# Agent Skills\n\nThe following skills are available. When a task matches a skill, use the `activate_skill` tool to load its full instructions before proceeding.\n\n");
        prompt.push_str(skills_catalog);
        prompt.push('\n');
    }

    prompt
}

fn history_to_claude_messages(
    history: &[StoredMessage],
    _bot_username: &str,
) -> Vec<Message> {
    let mut messages = Vec::new();

    for msg in history {
        let role = if msg.is_from_bot {
            "assistant"
        } else {
            "user"
        };

        let content = if msg.is_from_bot {
            msg.content.clone()
        } else {
            format!("[{}]: {}", msg.sender_name, msg.content)
        };

        // Merge consecutive messages of the same role
        if let Some(last) = messages.last_mut() {
            let last: &mut Message = last;
            if last.role == role {
                if let MessageContent::Text(t) = &mut last.content {
                    t.push('\n');
                    t.push_str(&content);
                }
                continue;
            }
        }

        messages.push(Message {
            role: role.into(),
            content: MessageContent::Text(content),
        });
    }

    // Ensure the last message is from user (Claude API requirement)
    if let Some(last) = messages.last() {
        if last.role == "assistant" {
            messages.pop();
        }
    }

    // Ensure we don't start with an assistant message
    while messages.first().map(|m| m.role.as_str()) == Some("assistant") {
        messages.remove(0);
    }

    messages
}

/// Split long text for Telegram's 4096-char limit.
/// Exposed for testing.
#[allow(dead_code)]
fn split_response_text(text: &str) -> Vec<String> {
    const MAX_LEN: usize = 4096;
    if text.len() <= MAX_LEN {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        let chunk_len = if remaining.len() <= MAX_LEN {
            remaining.len()
        } else {
            remaining[..MAX_LEN]
                .rfind('\n')
                .unwrap_or(MAX_LEN)
        };
        chunks.push(remaining[..chunk_len].to_string());
        remaining = &remaining[chunk_len..];
        if remaining.starts_with('\n') {
            remaining = &remaining[1..];
        }
    }
    chunks
}

pub(crate) async fn send_response(bot: &Bot, chat_id: ChatId, text: &str) {
    const MAX_LEN: usize = 4096;

    if text.len() <= MAX_LEN {
        let _ = bot.send_message(chat_id, text).await;
        return;
    }

    let mut remaining = text;
    while !remaining.is_empty() {
        let chunk_len = if remaining.len() <= MAX_LEN {
            remaining.len()
        } else {
            remaining[..MAX_LEN]
                .rfind('\n')
                .unwrap_or(MAX_LEN)
        };

        let chunk = &remaining[..chunk_len];
        let _ = bot.send_message(chat_id, chunk).await;
        remaining = &remaining[chunk_len..];

        if remaining.starts_with('\n') {
            remaining = &remaining[1..];
        }
    }
}

/// Extract text content from a Message for summarization/display.
fn message_to_text(msg: &Message) -> String {
    match &msg.content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text } => parts.push(text.clone()),
                    ContentBlock::ToolUse { name, input, .. } => {
                        parts.push(format!("[tool_use: {name}({})]", input));
                    }
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        let prefix = if is_error == &Some(true) {
                            "[tool_error]: "
                        } else {
                            "[tool_result]: "
                        };
                        // Truncate long tool results for summary
                        let truncated = if content.len() > 200 {
                            format!("{}...", &content[..200])
                        } else {
                            content.clone()
                        };
                        parts.push(format!("{prefix}{truncated}"));
                    }
                    ContentBlock::Image { .. } => {
                        parts.push("[image]".into());
                    }
                }
            }
            parts.join("\n")
        }
    }
}

/// Replace Image content blocks with text placeholders to avoid storing base64 data in sessions.
fn strip_images_for_session(messages: &mut [Message]) {
    for msg in messages.iter_mut() {
        if let MessageContent::Blocks(blocks) = &mut msg.content {
            for block in blocks.iter_mut() {
                if matches!(block, ContentBlock::Image { .. }) {
                    *block = ContentBlock::Text {
                        text: "[image was sent]".into(),
                    };
                }
            }
        }
    }
}

/// Compact old messages by summarizing them via Claude, keeping recent messages verbatim.
async fn compact_messages(
    claude: &ClaudeClient,
    messages: &[Message],
    keep_recent: usize,
) -> Vec<Message> {
    let total = messages.len();
    if total <= keep_recent {
        return messages.to_vec();
    }

    let split_at = total - keep_recent;
    let old_messages = &messages[..split_at];
    let recent_messages = &messages[split_at..];

    // Build text representation of old messages
    let mut summary_input = String::new();
    for msg in old_messages {
        let role = &msg.role;
        let text = message_to_text(msg);
        summary_input.push_str(&format!("[{role}]: {text}\n\n"));
    }

    // Truncate if very long
    if summary_input.len() > 20000 {
        summary_input.truncate(20000);
        summary_input.push_str("\n... (truncated)");
    }

    let summarize_prompt = "Summarize the following conversation concisely, preserving key facts, decisions, tool results, and context needed to continue the conversation. Be brief but thorough.";

    let summarize_messages = vec![Message {
        role: "user".into(),
        content: MessageContent::Text(format!(
            "{summarize_prompt}\n\n---\n\n{summary_input}"
        )),
    }];

    let summary = match claude
        .send_message("You are a helpful summarizer.", summarize_messages, None)
        .await
    {
        Ok(response) => response
            .content
            .iter()
            .filter_map(|b| match b {
                ResponseContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        Err(e) => {
            tracing::warn!("Compaction summarization failed: {e}, falling back to truncation");
            // Fallback: just keep recent messages
            return recent_messages.to_vec();
        }
    };

    // Build compacted message list: summary context + recent messages
    let mut compacted = vec![
        Message {
            role: "user".into(),
            content: MessageContent::Text(format!(
                "[Conversation Summary]\n{summary}"
            )),
        },
        Message {
            role: "assistant".into(),
            content: MessageContent::Text(
                "Understood, I have the conversation context. How can I help?".into(),
            ),
        },
    ];

    // Append recent messages, fixing role alternation
    for msg in recent_messages {
        if let Some(last) = compacted.last() {
            if last.role == msg.role {
                // Merge with previous to maintain alternation
                if let Some(last_mut) = compacted.last_mut() {
                    let existing = message_to_text(last_mut);
                    let new_text = message_to_text(msg);
                    last_mut.content =
                        MessageContent::Text(format!("{existing}\n{new_text}"));
                }
                continue;
            }
        }
        compacted.push(msg.clone());
    }

    // Ensure last message is from user
    if let Some(last) = compacted.last() {
        if last.role == "assistant" {
            compacted.pop();
        }
    }

    compacted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::StoredMessage;

    fn make_msg(id: &str, sender: &str, content: &str, is_bot: bool, ts: &str) -> StoredMessage {
        StoredMessage {
            id: id.into(),
            chat_id: 100,
            sender_name: sender.into(),
            content: content.into(),
            is_from_bot: is_bot,
            timestamp: ts.into(),
        }
    }

    #[test]
    fn test_history_to_claude_messages_basic() {
        let history = vec![
            make_msg("1", "alice", "hello", false, "2024-01-01T00:00:01Z"),
            make_msg("2", "bot", "hi there!", true, "2024-01-01T00:00:02Z"),
            make_msg("3", "alice", "how are you?", false, "2024-01-01T00:00:03Z"),
        ];
        let messages = history_to_claude_messages(&history, "bot");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[2].role, "user");

        if let MessageContent::Text(t) = &messages[0].content {
            assert_eq!(t, "[alice]: hello");
        } else {
            panic!("Expected Text content");
        }
        if let MessageContent::Text(t) = &messages[1].content {
            assert_eq!(t, "hi there!");
        } else {
            panic!("Expected Text content");
        }
    }

    #[test]
    fn test_history_to_claude_messages_merges_consecutive_user() {
        let history = vec![
            make_msg("1", "alice", "hello", false, "2024-01-01T00:00:01Z"),
            make_msg("2", "bob", "hi", false, "2024-01-01T00:00:02Z"),
            make_msg("3", "bot", "hey all!", true, "2024-01-01T00:00:03Z"),
            make_msg("4", "alice", "thanks", false, "2024-01-01T00:00:04Z"),
        ];
        let messages = history_to_claude_messages(&history, "bot");
        // Two user msgs merged, then assistant, then user -> 3 messages
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        if let MessageContent::Text(t) = &messages[0].content {
            assert!(t.contains("[alice]: hello"));
            assert!(t.contains("[bob]: hi"));
        } else {
            panic!("Expected Text content");
        }
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[2].role, "user");
    }

    #[test]
    fn test_history_to_claude_messages_removes_trailing_assistant() {
        let history = vec![
            make_msg("1", "alice", "hello", false, "2024-01-01T00:00:01Z"),
            make_msg("2", "bot", "response", true, "2024-01-01T00:00:02Z"),
        ];
        let messages = history_to_claude_messages(&history, "bot");
        // Trailing assistant message should be removed (Claude API requires last msg to be user)
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
    }

    #[test]
    fn test_history_to_claude_messages_removes_leading_assistant() {
        let history = vec![
            make_msg("1", "bot", "I said something", true, "2024-01-01T00:00:01Z"),
            make_msg("2", "alice", "hello", false, "2024-01-01T00:00:02Z"),
        ];
        let messages = history_to_claude_messages(&history, "bot");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
    }

    #[test]
    fn test_history_to_claude_messages_empty() {
        let messages = history_to_claude_messages(&[], "bot");
        assert!(messages.is_empty());
    }

    #[test]
    fn test_history_to_claude_messages_only_assistant() {
        let history = vec![
            make_msg("1", "bot", "hello", true, "2024-01-01T00:00:01Z"),
        ];
        let messages = history_to_claude_messages(&history, "bot");
        // Should be empty (leading + trailing assistant removed)
        assert!(messages.is_empty());
    }

    #[test]
    fn test_build_system_prompt_basic() {
        let prompt = build_system_prompt("testbot", "", 12345, "");
        assert!(prompt.contains("testbot"));
        assert!(prompt.contains("12345"));
        assert!(prompt.contains("bash commands"));
        assert!(!prompt.contains("# Memories"));
        assert!(!prompt.contains("# Agent Skills"));
    }

    #[test]
    fn test_build_system_prompt_with_memory() {
        let memory = "<global_memory>\nUser likes Rust\n</global_memory>";
        let prompt = build_system_prompt("testbot", memory, 42, "");
        assert!(prompt.contains("# Memories"));
        assert!(prompt.contains("User likes Rust"));
    }

    #[test]
    fn test_build_system_prompt_with_skills() {
        let catalog = "<available_skills>\n- pdf: Convert to PDF\n</available_skills>";
        let prompt = build_system_prompt("testbot", "", 42, catalog);
        assert!(prompt.contains("# Agent Skills"));
        assert!(prompt.contains("activate_skill"));
        assert!(prompt.contains("pdf: Convert to PDF"));
    }

    #[test]
    fn test_build_system_prompt_without_skills() {
        let prompt = build_system_prompt("testbot", "", 42, "");
        assert!(!prompt.contains("# Agent Skills"));
    }

    #[test]
    fn test_split_response_text_short() {
        let chunks = split_response_text("hello world");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "hello world");
    }

    #[test]
    fn test_split_response_text_long() {
        // Create a string longer than 4096 chars with newlines
        let mut text = String::new();
        for i in 0..200 {
            text.push_str(&format!("Line {i}: some content here that takes space\n"));
        }
        assert!(text.len() > 4096);

        let chunks = split_response_text(&text);
        assert!(chunks.len() > 1);
        // All chunks should be <= 4096
        for chunk in &chunks {
            assert!(chunk.len() <= 4096);
        }
        // Recombined should approximate original (newlines at split points are consumed)
        let total_len: usize = chunks.iter().map(|c| c.len()).sum();
        assert!(total_len > 0);
    }

    #[test]
    fn test_split_response_text_no_newlines() {
        // Long string without newlines - should split at MAX_LEN
        let text = "a".repeat(5000);
        let chunks = split_response_text(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 4096);
        assert_eq!(chunks[1].len(), 904);
    }

    #[test]
    fn test_guess_image_media_type_jpeg() {
        let data = vec![0xFF, 0xD8, 0xFF, 0xE0];
        assert_eq!(guess_image_media_type(&data), "image/jpeg");
    }

    #[test]
    fn test_guess_image_media_type_png() {
        let data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A];
        assert_eq!(guess_image_media_type(&data), "image/png");
    }

    #[test]
    fn test_guess_image_media_type_gif() {
        let data = b"GIF89a".to_vec();
        assert_eq!(guess_image_media_type(&data), "image/gif");
    }

    #[test]
    fn test_guess_image_media_type_webp() {
        let mut data = b"RIFF".to_vec();
        data.extend_from_slice(&[0, 0, 0, 0]); // file size
        data.extend_from_slice(b"WEBP");
        assert_eq!(guess_image_media_type(&data), "image/webp");
    }

    #[test]
    fn test_guess_image_media_type_unknown_defaults_jpeg() {
        let data = vec![0x00, 0x01, 0x02];
        assert_eq!(guess_image_media_type(&data), "image/jpeg");
    }

    #[test]
    fn test_base64_encode() {
        let data = b"hello world";
        let encoded = base64_encode(data);
        assert_eq!(encoded, "aGVsbG8gd29ybGQ=");
    }

    #[test]
    fn test_message_to_text_simple() {
        let msg = Message {
            role: "user".into(),
            content: MessageContent::Text("hello world".into()),
        };
        assert_eq!(message_to_text(&msg), "hello world");
    }

    #[test]
    fn test_message_to_text_blocks() {
        let msg = Message {
            role: "assistant".into(),
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "thinking".into(),
                },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ]),
        };
        let text = message_to_text(&msg);
        assert!(text.contains("thinking"));
        assert!(text.contains("[tool_use: bash("));
    }

    #[test]
    fn test_message_to_text_tool_result() {
        let msg = Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "file1.rs\nfile2.rs".into(),
                is_error: None,
            }]),
        };
        let text = message_to_text(&msg);
        assert!(text.contains("[tool_result]: file1.rs"));
    }

    #[test]
    fn test_message_to_text_image_block() {
        let msg = Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![
                ContentBlock::Image {
                    source: ImageSource {
                        source_type: "base64".into(),
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    },
                },
                ContentBlock::Text {
                    text: "what is this?".into(),
                },
            ]),
        };
        let text = message_to_text(&msg);
        assert!(text.contains("[image]"));
        assert!(text.contains("what is this?"));
    }

    #[test]
    fn test_strip_images_for_session() {
        let mut messages = vec![Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![
                ContentBlock::Image {
                    source: ImageSource {
                        source_type: "base64".into(),
                        media_type: "image/jpeg".into(),
                        data: "huge_base64_data".into(),
                    },
                },
                ContentBlock::Text {
                    text: "describe this".into(),
                },
            ]),
        }];

        strip_images_for_session(&mut messages);

        if let MessageContent::Blocks(blocks) = &messages[0].content {
            match &blocks[0] {
                ContentBlock::Text { text } => assert_eq!(text, "[image was sent]"),
                other => panic!("Expected Text, got {:?}", other),
            }
            match &blocks[1] {
                ContentBlock::Text { text } => assert_eq!(text, "describe this"),
                other => panic!("Expected Text, got {:?}", other),
            }
        } else {
            panic!("Expected Blocks content");
        }
    }

    #[test]
    fn test_strip_images_text_messages_unchanged() {
        let mut messages = vec![Message {
            role: "user".into(),
            content: MessageContent::Text("no images here".into()),
        }];

        strip_images_for_session(&mut messages);

        if let MessageContent::Text(t) = &messages[0].content {
            assert_eq!(t, "no images here");
        } else {
            panic!("Expected Text content");
        }
    }

    #[test]
    fn test_build_system_prompt_mentions_sub_agent() {
        let prompt = build_system_prompt("testbot", "", 12345, "");
        assert!(prompt.contains("sub_agent"));
    }
}
