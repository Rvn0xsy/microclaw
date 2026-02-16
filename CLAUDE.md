# MicroClaw

MicroClaw is a Rust multi-platform chat bot with a channel-agnostic core and platform adapters. It currently supports Telegram, Discord, Slack, Feishu/Lark, and Web, and can be extended to more platforms. It provides agentic tool execution, web search, scheduled tasks, and persistent memory. Inspired by [nanoclaw](https://github.com/gavrielc/nanoclaw/) (TypeScript/WhatsApp), incorporating some of its design ideas.

## Tech stack

Rust 2021, Tokio, teloxide 0.17, serenity 0.12, Anthropic Messages API (direct HTTP via reqwest), SQLite (rusqlite bundled), cron crate for scheduling.

## Directory overview

- `src/` -- Rust source for the bot binary
- `web/` -- Built-in Web UI (React + Vite). Compiled to `web/dist/` and embedded into the Rust binary via `include_dir!`. This is the chat interface and settings panel served by microclaw itself at runtime.
- `website/` -- **Separate git repository** (landing page + documentation site). Not part of the microclaw binary. Contains the public-facing marketing site and docs. Changes here have no effect on the bot.

## Project layout

- `src/main.rs` -- entry point, CLI
- `src/config.rs` -- YAML config loading
- `src/error.rs` -- error types (thiserror)
- `src/telegram.rs` -- message handler, agentic loop, session resume, context compaction, typing indicator, catch-up
- `src/discord.rs` -- Discord bot (serenity gateway, reuses process_with_claude)
- `src/channels/slack.rs` -- Slack bot (Socket Mode WebSocket, reuses process_with_agent)
- `src/channels/feishu.rs` -- Feishu/Lark bot (WebSocket long connection or webhook, reuses process_with_agent)
- `src/claude.rs` -- Anthropic API client, request/response types
- `src/db.rs` -- SQLite: chats, messages, scheduled_tasks, sessions tables
- `src/memory.rs` -- AGENTS.md memory system (global + per-chat)
- `src/scheduler.rs` -- background task scheduler (60s polling)
- `src/tools/mod.rs` -- Tool trait, ToolRegistry (17 tools), ToolRegistry::new_sub_agent (9 restricted tools)
- `src/tools/path_guard.rs` -- sensitive path blacklisting for file tools
- `src/tools/bash.rs` -- shell commands
- `src/tools/read_file.rs`, `write_file.rs`, `edit_file.rs` -- file operations (with path guard)
- `src/tools/glob.rs`, `grep.rs` -- file/content search (with path guard filtering)
- `src/tools/memory.rs` -- read_memory, write_memory
- `src/tools/web_search.rs` -- DuckDuckGo search
- `src/tools/browser.rs` -- headless browser automation (agent-browser CLI wrapper)
- `src/tools/web_fetch.rs` -- URL fetching with HTML stripping
- `src/tools/send_message.rs` -- mid-conversation messaging (all channels)
- `src/tools/schedule.rs` -- 5 scheduling tools
- `src/tools/sub_agent.rs` -- sub-agent tool with restricted tool registry

## Key patterns

- **Agentic loop** in `telegram.rs:process_with_claude`: call Claude -> if tool_use -> execute -> loop (up to `max_tool_iterations`)
- **Session resume**: full `Vec<Message>` (including tool_use/tool_result blocks) persisted in `sessions` table; on next invocation, loaded and appended with new user messages. `/reset` clears session.
- **Context compaction**: when session messages exceed `max_session_messages`, older messages are summarized via Claude and replaced with a compact summary + recent messages kept verbatim
- **Sub-agent**: `sub_agent` tool spawns a fresh agentic loop with 9 restricted tools (no send_message, write_memory, schedule, or recursive sub_agent)
- **Tool trait**: `name()`, `definition()` (JSON Schema), `execute(serde_json::Value) -> ToolResult`
- **Shared state**: `AppState` in `Arc`, tools hold `Bot` / `Arc<Database>` as needed
- **Group catch-up**: `db.get_messages_since_last_bot_response()` loads all messages since bot's last reply
- **Scheduler**: `tokio::spawn` loop, polls DB for due tasks, calls `process_with_claude` with `override_prompt`
- **Typing**: spawned task sends typing action every 4s, aborted when response is ready
- **Path guard**: sensitive paths (.ssh, .aws, .env, credentials, etc.) are blocked in file tools via `path_guard` module
- **Platform-extensible core**: Telegram/Discord/Slack/Feishu/Web adapters reuse `process_with_claude`; new platforms integrate through the same core loop
- **SOUL.md**: optional personality file injected into system prompt. Loaded from `soul_path` config, `data_dir/SOUL.md`, or `./SOUL.md`. Per-chat overrides via `data_dir/runtime/groups/{chat_id}/SOUL.md`

## Build & run

```sh
cargo build
cargo run -- start    # requires config.yaml with at least one enabled channel plus model credentials
cargo run -- setup    # interactive setup wizard to create config.yaml
cargo run -- help
```

## Configuration

MicroClaw uses `microclaw.config.yaml` (or `.yml`) for configuration. Override the path with `MICROCLAW_CONFIG` env var. See `microclaw.config.example.yaml` for all available fields.

## Soul (personality customization)

MicroClaw supports a `SOUL.md` file that defines the bot's personality, voice, values, and working style. The file content is injected into the system prompt, replacing the default "helpful AI assistant" identity.

**Loading priority** (first match wins):
1. `soul_path` in config (explicit path)
2. `<data_dir>/SOUL.md`
3. `./SOUL.md` (project root, ships with the repo as the default soul)

**Per-chat override**: place a `SOUL.md` at `<data_dir>/runtime/groups/<chat_id>/SOUL.md` to give a specific chat a different personality.

**Implementation**: `load_soul_content()` and `build_system_prompt()` in `src/agent_engine.rs`. The soul content is wrapped in `<soul>` XML tags in the system prompt.

## Adding a tool

1. Create `src/tools/my_tool.rs` implementing the `Tool` trait
2. Add `pub mod my_tool;` to `src/tools/mod.rs`
3. Register in `ToolRegistry::new()` with `Box::new(my_tool::MyTool::new(...))`

## Database

Four tables: `chats`, `messages`, `scheduled_tasks`, `sessions`. SQLite with WAL mode. Access via `Mutex<Connection>` in `Database` struct, shared as `Arc<Database>`.

## Important conventions

- All timestamps are ISO 8601 / RFC 3339 strings
- Cron expressions use 6-field format (sec min hour dom month dow)
- Messages are stored for all chats regardless of whether bot responds
- In groups, bot only responds to @mentions
- Consecutive same-role messages are merged before sending to Claude API
- Responses > 4096 chars are split at newline boundaries (Telegram), > 2000 chars for Discord, > 4000 chars for Slack/Feishu
