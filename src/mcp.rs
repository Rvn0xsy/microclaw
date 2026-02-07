use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

// --- JSON-RPC 2.0 types ---

#[derive(Debug, Serialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    #[allow(dead_code)]
    id: Option<serde_json::Value>,
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

// --- MCP config types ---

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct McpConfig {
    #[serde(rename = "mcpServers")]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub server_name: String,
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

// --- MCP server connection ---

struct McpServerInner {
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    _child: Child,
    next_id: u64,
}

pub struct McpServer {
    name: String,
    inner: Mutex<McpServerInner>,
    tools: Vec<McpToolInfo>,
}

impl McpServer {
    pub async fn connect(name: &str, config: &McpServerConfig) -> Result<Self, String> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args);
        cmd.envs(&config.env);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn MCP server '{name}': {e}"))?;

        let stdin = child.stdin.take().ok_or("Failed to get stdin")?;
        let stdout = child.stdout.take().ok_or("Failed to get stdout")?;
        let stdout = BufReader::new(stdout);

        let mut server = McpServer {
            name: name.to_string(),
            inner: Mutex::new(McpServerInner {
                stdin,
                stdout,
                _child: child,
                next_id: 1,
            }),
            tools: Vec::new(),
        };

        // Initialize handshake
        server.initialize().await?;

        // Discover tools
        let tools = server.list_tools().await?;
        server.tools = tools;

        Ok(server)
    }

    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let mut inner = self.inner.lock().await;
        let id = inner.next_id;
        inner.next_id += 1;

        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        };

        let mut json = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        json.push('\n');

        inner
            .stdin
            .write_all(json.as_bytes())
            .await
            .map_err(|e| format!("Write error: {e}"))?;
        inner
            .stdin
            .flush()
            .await
            .map_err(|e| format!("Flush error: {e}"))?;

        // Read response lines until we get a valid JSON-RPC response
        let mut line = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            line.clear();
            let read_result = tokio::time::timeout_at(
                deadline,
                inner.stdout.read_line(&mut line),
            )
            .await;

            match read_result {
                Err(_) => return Err("MCP server response timeout (120s)".into()),
                Ok(Err(e)) => return Err(format!("Read error: {e}")),
                Ok(Ok(0)) => return Err("MCP server closed connection".into()),
                Ok(Ok(_)) => {}
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Try to parse as JSON-RPC response
            if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
                // Skip notifications (no id or null id)
                let is_response = match &response.id {
                    Some(serde_json::Value::Number(n)) => n.as_u64() == Some(id),
                    _ => response.result.is_some() || response.error.is_some(),
                };
                if !is_response {
                    continue;
                }
                if let Some(err) = response.error {
                    return Err(format!("MCP error ({}): {}", err.code, err.message));
                }
                return Ok(response.result.unwrap_or(serde_json::Value::Null));
            }
            // Not valid JSON, skip (could be log output)
        }
    }

    async fn initialize(&self) -> Result<(), String> {
        let params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "microclaw",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        self.send_request("initialize", Some(params)).await?;

        // Send initialized notification (fire-and-forget, no id)
        let mut inner = self.inner.lock().await;
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let mut json = serde_json::to_string(&notification).map_err(|e| e.to_string())?;
        json.push('\n');
        inner
            .stdin
            .write_all(json.as_bytes())
            .await
            .map_err(|e| format!("Write error: {e}"))?;
        inner
            .stdin
            .flush()
            .await
            .map_err(|e| format!("Flush error: {e}"))?;

        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<McpToolInfo>, String> {
        let result = self
            .send_request("tools/list", Some(serde_json::json!({})))
            .await?;

        let tools_value = result.get("tools").ok_or("No tools in response")?;
        let tools_array = tools_value.as_array().ok_or("tools is not an array")?;

        let mut tools = Vec::new();
        for tool in tools_array {
            let name = tool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input_schema = tool.get("inputSchema").cloned().unwrap_or_else(|| {
                serde_json::json!({"type": "object", "properties": {}})
            });

            tools.push(McpToolInfo {
                server_name: self.name.clone(),
                name,
                description,
                input_schema,
            });
        }

        Ok(tools)
    }

    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, String> {
        let params = serde_json::json!({
            "name": tool_name,
            "arguments": arguments
        });

        let result = self.send_request("tools/call", Some(params)).await?;

        // Check for isError flag
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Extract text content
        if let Some(content) = result.get("content") {
            if let Some(array) = content.as_array() {
                let mut output = String::new();
                for item in array {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        if !output.is_empty() {
                            output.push('\n');
                        }
                        output.push_str(text);
                    }
                }
                if is_error {
                    return Err(output);
                }
                return Ok(output);
            }
        }

        // Fallback: return whole result as JSON
        let output = serde_json::to_string_pretty(&result).unwrap_or_default();
        if is_error {
            Err(output)
        } else {
            Ok(output)
        }
    }

    pub fn tools(&self) -> &[McpToolInfo] {
        &self.tools
    }

    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        &self.name
    }
}

// --- MCP manager ---

pub struct McpManager {
    servers: Vec<Arc<McpServer>>,
}

impl McpManager {
    pub async fn from_config_file(path: &str) -> Self {
        let config_str = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => {
                // Config file not found is normal — MCP is optional
                return McpManager {
                    servers: Vec::new(),
                };
            }
        };

        let config: McpConfig = match serde_json::from_str(&config_str) {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to parse MCP config {path}: {e}");
                return McpManager {
                    servers: Vec::new(),
                };
            }
        };

        let mut servers = Vec::new();
        for (name, server_config) in &config.mcp_servers {
            info!("Connecting to MCP server '{name}'...");
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                McpServer::connect(name, server_config),
            )
            .await
            {
                Ok(Ok(server)) => {
                    info!(
                        "MCP server '{name}' connected ({} tools)",
                        server.tools().len()
                    );
                    servers.push(Arc::new(server));
                }
                Ok(Err(e)) => {
                    warn!("Failed to connect MCP server '{name}': {e}");
                }
                Err(_) => {
                    warn!("MCP server '{name}' connection timed out (30s)");
                }
            }
        }

        McpManager { servers }
    }

    #[allow(dead_code)]
    pub fn servers(&self) -> &[Arc<McpServer>] {
        &self.servers
    }

    pub fn all_tools(&self) -> Vec<(Arc<McpServer>, McpToolInfo)> {
        let mut tools = Vec::new();
        for server in &self.servers {
            for tool in server.tools() {
                tools.push((server.clone(), tool.clone()));
            }
        }
        tools
    }
}
