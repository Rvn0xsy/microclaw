mod claude;
mod config;
mod db;
mod error;
mod mcp;
mod memory;
mod scheduler;
mod skills;
mod telegram;
mod tools;
mod transcribe;
mod whatsapp;

use config::Config;
use tracing::info;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_help() {
    println!(
        r#"MicroClaw v{VERSION} — Agentic AI assistant for Telegram

USAGE:
    microclaw <COMMAND>

COMMANDS:
    start       Start the Telegram bot
    help        Show this help message

FEATURES:
    - Agentic tool use (bash, files, search, memory)
    - Web search and page fetching
    - Image/photo understanding (Claude Vision)
    - Voice message transcription (OpenAI Whisper)
    - Scheduled/recurring tasks with timezone support
    - Task execution history/run logs
    - Chat export to markdown
    - Mid-conversation message sending
    - Group chat catch-up (reads all messages since last reply)
    - Group allowlist (restrict which groups can use the bot)
    - Continuous typing indicator
    - MCP (Model Context Protocol) server integration
    - WhatsApp Cloud API support

SETUP:
    1. Copy .env.example to .env
    2. Fill in the required values:

       TELEGRAM_BOT_TOKEN   Bot token from @BotFather
       ANTHROPIC_API_KEY    API key from console.anthropic.com
       BOT_USERNAME         Your bot's username (without @)

    3. Run: microclaw start

OPTIONAL ENV VARS:
    CLAUDE_MODEL             Model to use (default: claude-sonnet-4-20250514)
    DATA_DIR                 Data directory (default: ./data)
    MAX_TOKENS               Max tokens per response (default: 8192)
    MAX_TOOL_ITERATIONS      Max tool loop iterations (default: 25)
    MAX_HISTORY_MESSAGES     Chat history context size (default: 50)
    OPENAI_API_KEY           OpenAI API key for voice transcription (optional)
    TIMEZONE                 IANA timezone for scheduling (default: UTC)
    ALLOWED_GROUPS           Comma-separated chat IDs to allow (empty = all)
    RUST_LOG                 Log level, e.g. debug, info (default: info)

WHATSAPP (optional):
    WHATSAPP_ACCESS_TOKEN    Meta API access token
    WHATSAPP_PHONE_NUMBER_ID Phone number ID from Meta dashboard
    WHATSAPP_VERIFY_TOKEN    Webhook verification token (you choose)
    WHATSAPP_WEBHOOK_PORT    Webhook server port (default: 8080)

MCP (optional):
    Place a mcp.json file in DATA_DIR to connect MCP servers.
    See https://modelcontextprotocol.io for details.

EXAMPLES:
    microclaw start          Start the bot
    microclaw help           Show this message

AUTHOR:
    everettjf — https://github.com/everettjf"#
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str());

    match command {
        Some("start") => {}
        Some("help" | "--help" | "-h") | None => {
            print_help();
            return Ok(());
        }
        Some(unknown) => {
            eprintln!("Unknown command: {unknown}\n");
            print_help();
            std::process::exit(1);
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let config = Config::from_env()?;
    info!("Starting MicroClaw bot...");

    let db = db::Database::new(&config.data_dir)?;
    info!("Database initialized");

    let memory_manager = memory::MemoryManager::new(&config.data_dir);
    info!("Memory manager initialized");

    let skill_manager = skills::SkillManager::new(&config.data_dir);
    let discovered = skill_manager.discover_skills();
    info!("Skill manager initialized ({} skills discovered)", discovered.len());

    // Initialize MCP servers (optional, configured via data_dir/mcp.json)
    let mcp_config_path = std::path::Path::new(&config.data_dir)
        .join("mcp.json")
        .to_string_lossy()
        .to_string();
    let mcp_manager = mcp::McpManager::from_config_file(&mcp_config_path).await;
    let mcp_tool_count: usize = mcp_manager.all_tools().len();
    if mcp_tool_count > 0 {
        info!("MCP initialized: {} tools available", mcp_tool_count);
    }

    telegram::run_bot(config, db, memory_manager, skill_manager, mcp_manager).await?;

    Ok(())
}
