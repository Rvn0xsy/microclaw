use crate::error::MicroClawError;

#[derive(Clone, Debug)]
pub struct Config {
    pub telegram_bot_token: String,
    pub anthropic_api_key: String,
    pub bot_username: String,
    pub claude_model: String,
    pub data_dir: String,
    pub max_tokens: u32,
    pub max_tool_iterations: usize,
    pub max_history_messages: usize,
    pub openai_api_key: Option<String>,
    pub timezone: String,
    pub allowed_groups: Vec<i64>,
    pub max_session_messages: usize,
    pub compact_keep_recent: usize,
    pub whatsapp_access_token: Option<String>,
    pub whatsapp_phone_number_id: Option<String>,
    pub whatsapp_verify_token: Option<String>,
    pub whatsapp_webhook_port: u16,
}

impl Config {
    pub fn from_env() -> Result<Self, MicroClawError> {
        dotenvy::dotenv().ok();

        let telegram_bot_token = std::env::var("TELEGRAM_BOT_TOKEN")
            .map_err(|_| MicroClawError::Config("TELEGRAM_BOT_TOKEN not set".into()))?;
        let anthropic_api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| MicroClawError::Config("ANTHROPIC_API_KEY not set".into()))?;
        let bot_username = std::env::var("BOT_USERNAME")
            .map_err(|_| MicroClawError::Config("BOT_USERNAME not set".into()))?;

        let claude_model = std::env::var("CLAUDE_MODEL")
            .unwrap_or_else(|_| "claude-sonnet-4-20250514".into());
        let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "./data".into());
        let max_tokens = std::env::var("MAX_TOKENS")
            .unwrap_or_else(|_| "8192".into())
            .parse::<u32>()
            .map_err(|e| MicroClawError::Config(format!("Invalid MAX_TOKENS: {e}")))?;
        let max_tool_iterations = std::env::var("MAX_TOOL_ITERATIONS")
            .unwrap_or_else(|_| "25".into())
            .parse::<usize>()
            .map_err(|e| MicroClawError::Config(format!("Invalid MAX_TOOL_ITERATIONS: {e}")))?;
        let max_history_messages = std::env::var("MAX_HISTORY_MESSAGES")
            .unwrap_or_else(|_| "50".into())
            .parse::<usize>()
            .map_err(|e| MicroClawError::Config(format!("Invalid MAX_HISTORY_MESSAGES: {e}")))?;

        let openai_api_key = std::env::var("OPENAI_API_KEY").ok().filter(|s| !s.is_empty());

        let timezone = std::env::var("TIMEZONE").unwrap_or_else(|_| "UTC".into());
        // Validate timezone parses
        timezone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| MicroClawError::Config(format!("Invalid TIMEZONE: {timezone}")))?;

        let max_session_messages = std::env::var("MAX_SESSION_MESSAGES")
            .unwrap_or_else(|_| "40".into())
            .parse::<usize>()
            .map_err(|e| MicroClawError::Config(format!("Invalid MAX_SESSION_MESSAGES: {e}")))?;
        let compact_keep_recent = std::env::var("COMPACT_KEEP_RECENT")
            .unwrap_or_else(|_| "20".into())
            .parse::<usize>()
            .map_err(|e| MicroClawError::Config(format!("Invalid COMPACT_KEEP_RECENT: {e}")))?;

        let whatsapp_access_token = std::env::var("WHATSAPP_ACCESS_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let whatsapp_phone_number_id = std::env::var("WHATSAPP_PHONE_NUMBER_ID")
            .ok()
            .filter(|s| !s.is_empty());
        let whatsapp_verify_token = std::env::var("WHATSAPP_VERIFY_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let whatsapp_webhook_port = std::env::var("WHATSAPP_WEBHOOK_PORT")
            .unwrap_or_else(|_| "8080".into())
            .parse::<u16>()
            .map_err(|e| MicroClawError::Config(format!("Invalid WHATSAPP_WEBHOOK_PORT: {e}")))?;

        let allowed_groups = std::env::var("ALLOWED_GROUPS")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                s.trim()
                    .parse::<i64>()
                    .map_err(|e| MicroClawError::Config(format!("Invalid ALLOWED_GROUPS entry '{s}': {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Config {
            telegram_bot_token,
            anthropic_api_key,
            bot_username,
            claude_model,
            data_dir,
            max_tokens,
            max_tool_iterations,
            max_history_messages,
            openai_api_key,
            timezone,
            allowed_groups,
            max_session_messages,
            compact_keep_recent,
            whatsapp_access_token,
            whatsapp_phone_number_id,
            whatsapp_verify_token,
            whatsapp_webhook_port,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_struct_clone_and_debug() {
        let config = Config {
            telegram_bot_token: "tok".into(),
            anthropic_api_key: "key".into(),
            bot_username: "bot".into(),
            claude_model: "model".into(),
            data_dir: "./data".into(),
            max_tokens: 8192,
            max_tool_iterations: 25,
            max_history_messages: 50,
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
        let cloned = config.clone();
        assert_eq!(cloned.telegram_bot_token, "tok");
        assert_eq!(cloned.max_tokens, 8192);
        assert_eq!(cloned.max_tool_iterations, 25);
        assert_eq!(cloned.max_history_messages, 50);
        assert!(cloned.openai_api_key.is_none());
        assert_eq!(cloned.timezone, "UTC");
        assert!(cloned.allowed_groups.is_empty());
        assert_eq!(cloned.max_session_messages, 40);
        assert_eq!(cloned.compact_keep_recent, 20);
        // Debug should not panic
        let _ = format!("{:?}", config);
    }

    #[test]
    fn test_config_default_values() {
        // We can't easily call from_env() in tests without setting env vars,
        // but we can verify the struct works with expected defaults
        let config = Config {
            telegram_bot_token: "test".into(),
            anthropic_api_key: "test".into(),
            bot_username: "testbot".into(),
            claude_model: "claude-sonnet-4-20250514".into(),
            data_dir: "./data".into(),
            max_tokens: 8192,
            max_tool_iterations: 25,
            max_history_messages: 50,
            openai_api_key: Some("sk-test".into()),
            timezone: "US/Eastern".into(),
            allowed_groups: vec![123, 456],
            max_session_messages: 40,
            compact_keep_recent: 20,
            whatsapp_access_token: None,
            whatsapp_phone_number_id: None,
            whatsapp_verify_token: None,
            whatsapp_webhook_port: 8080,
        };
        assert_eq!(config.claude_model, "claude-sonnet-4-20250514");
        assert_eq!(config.data_dir, "./data");
        assert_eq!(config.openai_api_key.as_deref(), Some("sk-test"));
        assert_eq!(config.timezone, "US/Eastern");
        assert_eq!(config.allowed_groups, vec![123, 456]);
    }
}
