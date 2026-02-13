use std::sync::Arc;

use anyhow::anyhow;
use teloxide::prelude::*;
use tracing::info;

use crate::config::Config;
use crate::db::Database;
use crate::llm::LlmProvider;
use crate::memory::MemoryManager;
use crate::skills::SkillManager;
use crate::tools::ToolRegistry;

pub struct AppState {
    pub config: Config,
    pub telegram_bot: Option<Bot>,
    pub db: Arc<Database>,
    pub memory: MemoryManager,
    pub skills: SkillManager,
    pub llm: Box<dyn LlmProvider>,
    pub tools: ToolRegistry,
}

pub async fn run(
    config: Config,
    db: Database,
    memory: MemoryManager,
    skills: SkillManager,
    mcp_manager: crate::mcp::McpManager,
) -> anyhow::Result<()> {
    let telegram_bot = if config.telegram_bot_token.trim().is_empty() {
        None
    } else {
        Some(Bot::new(&config.telegram_bot_token))
    };

    let db = Arc::new(db);
    let llm = crate::llm::create_provider(&config);
    let mut tools = ToolRegistry::new(&config, telegram_bot.clone(), db.clone());

    for (server, tool_info) in mcp_manager.all_tools() {
        tools.add_tool(Box::new(crate::tools::mcp::McpTool::new(server, tool_info)));
    }

    let state = Arc::new(AppState {
        config,
        telegram_bot,
        db,
        memory,
        skills,
        llm,
        tools,
    });

    crate::scheduler::spawn_scheduler(state.clone());
    if let Some(ref token) = state.config.discord_bot_token {
        if !token.trim().is_empty() {
            let discord_state = state.clone();
            let token = token.clone();
            info!("Starting Discord bot");
            tokio::spawn(async move {
                crate::discord::start_discord_bot(discord_state, &token).await;
            });
        }
    }

    if state.config.web_enabled {
        let web_state = state.clone();
        info!(
            "Starting Web UI server on {}:{}",
            state.config.web_host, state.config.web_port
        );
        tokio::spawn(async move {
            crate::web::start_web_server(web_state).await;
        });
    }

    if let Some(bot) = state.telegram_bot.clone() {
        crate::telegram::start_telegram_bot(state, bot).await
    } else if state.config.web_enabled
        || state
            .config
            .discord_bot_token
            .as_deref()
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false)
    {
        info!("Running without Telegram adapter; waiting for other channels");
        tokio::signal::ctrl_c()
            .await
            .map_err(|e| anyhow!("Failed to listen for Ctrl-C: {e}"))?;
        Ok(())
    } else {
        Err(anyhow!(
            "No channel is enabled. Configure Telegram, Discord, or web_enabled=true."
        ))
    }
}
