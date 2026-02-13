# AGENTS.md

## Project overview

MicroClaw is a Rust multi-platform chat bot with a channel-agnostic core plus platform adapters. It currently supports Telegram, Discord, and Web chats, and can be extended to additional platforms. It provides agentic tool execution, web browsing, scheduled tasks, and persistent memory. Inspired by [nanoclaw](https://github.com/gavrielc/nanoclaw/) (TypeScript/WhatsApp), incorporating some of its design ideas.

## Tech stack

- **Language:** Rust (2021 edition)
- **Async runtime:** Tokio
- **Telegram:** teloxide 0.17
- **Discord:** serenity 0.12
- **AI:** Anthropic Messages API via reqwest (direct HTTP, no SDK)
- **Database:** SQLite via rusqlite (bundled)
- **Serialization:** serde + serde_json
- **Scheduling:** cron 0.13 (6-field cron expressions)
- **Web:** reqwest for HTTP, regex for HTML parsing, urlencoding for query params

## Project structure

```
src/
    main.rs          -- Entry point. Initializes config, DB, memory manager, starts bot.
    config.rs        -- Loads all settings from microclaw.config.yaml.
    error.rs         -- MicroClawError enum (thiserror). All error variants for the app.
    telegram.rs      -- Telegram message handler. Contains the shared agentic tool-use loop
                        (process_with_claude), session resume (load/save full message
                        state), context compaction (summarize old messages), continuous
                        typing indicator, group chat catch-up, and response splitting.
    discord.rs       -- Discord message handler via serenity gateway. Reuses
                        process_with_claude and shared tool/session/memory flow.
    claude.rs        -- Anthropic Messages API client. Request/response types, HTTP calls
                        with retry on 429.
    db.rs            -- SQLite database. Four tables: chats, messages, scheduled_tasks,
                        sessions. Uses Mutex<Connection> for thread safety. Shared as
                        Arc<Database>.
    memory.rs        -- MemoryManager. Reads/writes AGENTS.md files at global and per-chat
                        scopes. Builds memory context injected into system prompts.
    scheduler.rs     -- Background scheduler. Spawns a tokio task that polls every 60s
                        for due tasks, executes the agent loop, sends results to chat.
    tools/
        mod.rs       -- Tool trait (async_trait), ToolRegistry, ToolResult type.
                        Registry constructor takes (data_dir, Bot, Arc<Database>).
                        17 tools registered total. new_sub_agent() creates restricted
                        registry with 9 tools (no side-effect or recursive tools).
        bash.rs      -- Executes shell commands via tokio::process::Command.
        read_file.rs -- Reads files with line numbers, offset/limit support.
        write_file.rs-- Writes files, auto-creates parent directories.
        edit_file.rs -- Find/replace editing. Validates old_string is unique.
        glob.rs      -- File pattern matching via the glob crate.
        grep.rs      -- Recursive regex search with directory traversal.
        memory.rs    -- read_memory / write_memory tools for AGENTS.md persistence.
        web_search.rs-- DuckDuckGo HTML search. GET html.duckduckgo.com/html/?q=...,
                        regex parse result__a (links) and result__snippet (descriptions).
        web_fetch.rs -- Fetch URL, strip HTML tags via regex, return plain text (max 20KB).
        send_message.rs -- Send message mid-conversation (Telegram/Discord). Holds
                           platform clients as needed. Chat/channel identifiers are
                           passed via tool input.
        schedule.rs  -- 5 scheduling tools: schedule_task, list_scheduled_tasks,
                        pause_scheduled_task, resume_scheduled_task, cancel_scheduled_task.
                        Each holds Arc<Database>.
        sub_agent.rs -- Sub-agent tool. Spawns a fresh agentic loop with restricted
                        tools (9 tools: bash, file ops, glob, grep, web, read_memory).
                        No send_message, write_memory, schedule, or recursive sub_agent.
```

Top-level web directories:

- `web/` -- MicroClaw built-in Web service/Web UI code used by this repository at runtime.
- `website/` -- Separate website repository checked out alongside this project, used for landing page and documentation site content (not the in-app Web service).

## Key patterns

### Agentic tool-use loop (`telegram.rs:process_with_claude`)

The core loop:
1. Try loading saved session (full `Vec<Message>` with tool blocks) from `sessions` table
   - If session exists: deserialize, append new user messages since `updated_at`
   - If no session: fall back to DB history:
     - Private chats: last N messages (`get_recent_messages`)
     - Groups: all messages since last bot response (`get_messages_since_last_bot_response`)
2. Build system prompt with memory context and chat_id
3. If `override_prompt` is set (from scheduler), append as user message
4. Compact if messages exceed `max_session_messages` (summarize old messages via Claude, keep recent verbatim)
5. Call Claude API with tool definitions
6. If `stop_reason == "tool_use"` -> execute tools -> append results -> loop back to step 5
7. If `stop_reason == "end_turn"` -> extract text -> strip image base64 -> save session -> return
8. Loop up to `max_tool_iterations` times

### Session resume (`db.rs` sessions table + `telegram.rs`)

Full conversation state (including tool_use and tool_result blocks) is serialized to JSON and persisted in the `sessions` table after each agentic loop. On the next invocation, the session is loaded and new user messages are appended. Image base64 data is stripped before saving to avoid bloat. Send `/reset` to clear a session.

### Context compaction (`telegram.rs:compact_messages`)

When session message count exceeds `max_session_messages` (default 40):
1. Split messages into old (to summarize) and recent (to keep, default 20)
2. Call Claude with a summarization prompt (no tools)
3. Replace old messages with `[Conversation Summary]` + assistant ack
4. Append recent messages with role alternation fix
5. On API failure: fall back to simple truncation (discard old, keep recent)

### Sub-agent (`tools/sub_agent.rs`)

The `sub_agent` tool spawns an independent agentic loop (max 10 iterations) with a restricted `ToolRegistry` (9 tools). Excluded: send_message, write_memory, schedule tools, export_chat, sub_agent (prevents recursion). Used for delegating self-contained research or coding tasks.

### Typing indicator (`telegram.rs:handle_message`)

A `tokio::spawn` task sends `ChatAction::Typing` every 4 seconds. The handle is `abort()`ed when processing completes. This keeps the typing indicator visible for the entire duration of multi-tool interactions.

### Scheduler (`scheduler.rs`)

Spawned in `run_bot()` as a background task:
1. Sleep 60 seconds
2. Query `scheduled_tasks WHERE status='active' AND next_run <= now`
3. For each due task, call `process_with_claude(state, chat_id, "scheduler", "private", Some(prompt))`
4. Send response to chat
5. Update task: for cron tasks compute next_run, for one-shot tasks set status='completed'

### Tool system (`tools/mod.rs`)

All tools implement the `Tool` trait:
```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, input: serde_json::Value) -> ToolResult;
}
```

`ToolRegistry` holds all tools and dispatches execution by name. Tool definitions are passed to Claude as JSON Schema.

Constructor signatures:
- `ToolRegistry::new(config: &Config, bot: Bot, db: Arc<Database>)` -- full registry (17 tools)
- `ToolRegistry::new_sub_agent(config: &Config)` -- restricted registry (9 tools, no side-effects/recursion)

### Memory system (`memory.rs`)

Two scopes:
- **Global:** `data/groups/AGENTS.md` -- shared across all chats
- **Per-chat:** `data/groups/{chat_id}/AGENTS.md` -- specific to one conversation

Memory content is injected into the system prompt wrapped in `<global_memory>` / `<chat_memory>` XML tags. Claude reads/writes memory via the `read_memory` and `write_memory` tools.

### Database (`db.rs`)

Four tables:
- `chats` -- chat metadata (id, title, type, last message time)
- `messages` -- all messages (id, chat_id, sender, content, is_from_bot, timestamp)
- `scheduled_tasks` -- scheduled tasks (id, chat_id, prompt, schedule_type, schedule_value, next_run, last_run, status, created_at)
- `sessions` -- session state (chat_id PK, messages_json, updated_at) for session resume

Uses WAL mode for performance. `Database` struct wraps `Mutex<Connection>`, shared as `Arc<Database>`.

### Claude API (`claude.rs`)

Direct HTTP to `https://api.anthropic.com/v1/messages` with:
- `x-api-key` header for auth
- `anthropic-version: 2023-06-01` header
- Exponential backoff retry on HTTP 429 (up to 3 attempts)
- Content blocks use tagged enums: `Text`, `ToolUse`, `ToolResult`

### Message handling (platform-specific)

- **Telegram private chats:** always respond
- **Telegram groups:** only respond when `@bot_username` is mentioned
- **Discord DMs:** always respond
- **Discord server channels:** respond on mention (and optional channel whitelist)
- All messages are stored regardless of whether the bot responds
- Consecutive same-role messages are merged before sending to Claude
- Responses are split at channel limits (Telegram 4096, Discord 2000)
- Empty responses are not sent (agent may have used send_message tool)

## Build and run

```sh
cargo build              # dev build
cargo build --release    # release build
cargo run -- start       # run (requires microclaw.config.yaml)
cargo run -- help        # CLI help
```

Requires a `microclaw.config.yaml` with at least one enabled channel (`telegram_bot_token` or `discord_bot_token`, or `web_enabled=true`) plus model credentials.

## Adding a new tool

1. Create `src/tools/my_tool.rs`
2. Implement the `Tool` trait (name, definition with JSON Schema, execute)
3. Add `pub mod my_tool;` to `src/tools/mod.rs`
4. Register it in `ToolRegistry::new()` with `Box::new(my_tool::MyTool::new(...))`
5. If the tool needs `Bot` or `Arc<Database>`, add a constructor that accepts them

## Common tasks

- **Change the model:** set `model` in `microclaw.config.yaml`
- **Increase context:** set `max_history_messages` higher in `microclaw.config.yaml` (costs more tokens)
- **Increase tool iterations:** set `max_tool_iterations` higher in `microclaw.config.yaml` (default: 100)
- **Debug logging:** run with `RUST_LOG=debug cargo run -- start`
- **Reset memory:** delete files under `microclaw.data/runtime/groups/`
- **Reset all data:** delete the `microclaw.data/` directory
- **Cancel all tasks:** `sqlite3 microclaw.data/runtime/microclaw.db "UPDATE scheduled_tasks SET status='cancelled' WHERE status='active';"`
- **Tune compaction:** set `max_session_messages` (default 40) and `compact_keep_recent` (default 20) in `microclaw.config.yaml`
- **Reset a chat session:** send `/reset` in chat, or `sqlite3 microclaw.data/runtime/microclaw.db "DELETE FROM sessions WHERE chat_id=XXXX;"`
