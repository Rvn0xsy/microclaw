use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

const DEFAULT_PROTOCOL_VERSION: &str = "2025-11-05";

// --- JSON-RPC 2.0 types ---

#[derive(Debug, Serialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
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

fn default_transport() -> String {
    "stdio".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default, alias = "protocolVersion")]
    pub protocol_version: Option<String>,
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,

    // stdio transport
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,

    // streamable_http transport
    #[serde(default, alias = "url")]
    pub endpoint: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct McpConfig {
    #[serde(default, alias = "defaultProtocolVersion")]
    pub default_protocol_version: Option<String>,
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

struct McpStdioInner {
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    _child: Child,
    next_id: u64,
}

struct McpHttpInner {
    client: reqwest::Client,
    endpoint: String,
    headers: HashMap<String, String>,
    next_id: u64,
}

enum McpTransport {
    Stdio(Mutex<McpStdioInner>),
    StreamableHttp(Mutex<McpHttpInner>),
}

pub struct McpServer {
    name: String,
    requested_protocol: String,
    negotiated_protocol: String,
    transport: McpTransport,
    tools: Vec<McpToolInfo>,
}

impl McpServer {
    pub async fn connect(
        name: &str,
        config: &McpServerConfig,
        default_protocol_version: Option<&str>,
    ) -> Result<Self, String> {
        let requested_protocol = config
            .protocol_version
            .clone()
            .or_else(|| default_protocol_version.map(|v| v.to_string()))
            .unwrap_or_else(|| DEFAULT_PROTOCOL_VERSION.to_string());

        let timeout_secs = config.request_timeout_secs.unwrap_or(120);
        let transport_name = config.transport.trim().to_ascii_lowercase();

        let transport = match transport_name.as_str() {
            "stdio" | "" => {
                if config.command.trim().is_empty() {
                    return Err(format!(
                        "MCP server '{name}' requires `command` when transport=stdio"
                    ));
                }

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

                McpTransport::Stdio(Mutex::new(McpStdioInner {
                    stdin,
                    stdout,
                    _child: child,
                    next_id: 1,
                }))
            }
            "streamable_http" | "http" => {
                if config.endpoint.trim().is_empty() {
                    return Err(format!(
                        "MCP server '{name}' requires `endpoint` when transport=streamable_http"
                    ));
                }

                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(timeout_secs))
                    .build()
                    .map_err(|e| format!("Failed to build HTTP client for MCP '{name}': {e}"))?;

                McpTransport::StreamableHttp(Mutex::new(McpHttpInner {
                    client,
                    endpoint: config.endpoint.clone(),
                    headers: config.headers.clone(),
                    next_id: 1,
                }))
            }
            other => {
                return Err(format!(
                    "MCP server '{name}' has unsupported transport '{other}'"
                ));
            }
        };

        let mut server = McpServer {
            name: name.to_string(),
            requested_protocol: requested_protocol.clone(),
            negotiated_protocol: requested_protocol,
            transport,
            tools: Vec::new(),
        };

        // Initialize handshake and negotiate protocol
        let negotiated = server.initialize().await?;
        server.negotiated_protocol = negotiated;

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
        match &self.transport {
            McpTransport::Stdio(inner) => {
                let mut inner = inner.lock().await;
                let id = inner.next_id;
                inner.next_id += 1;

                let request = JsonRpcRequest {
                    jsonrpc: "2.0".to_string(),
                    id: Some(id),
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
                    let read_result =
                        tokio::time::timeout_at(deadline, inner.stdout.read_line(&mut line)).await;

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

                    if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
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
                }
            }
            McpTransport::StreamableHttp(inner) => {
                let mut inner = inner.lock().await;
                let id = inner.next_id;
                inner.next_id += 1;

                let request = JsonRpcRequest {
                    jsonrpc: "2.0".to_string(),
                    id: Some(id),
                    method: method.to_string(),
                    params,
                };

                let mut req = inner.client.post(&inner.endpoint).json(&request);
                for (k, v) in &inner.headers {
                    req = req.header(k, v);
                }

                let response = req
                    .send()
                    .await
                    .map_err(|e| format!("HTTP request failed: {e}"))?;
                let status = response.status();
                let body: serde_json::Value = response
                    .json()
                    .await
                    .map_err(|e| format!("Failed to parse HTTP MCP response: {e}"))?;

                if !status.is_success() {
                    return Err(format!("HTTP MCP request failed with {status}: {body}"));
                }

                if let Ok(parsed) = serde_json::from_value::<JsonRpcResponse>(body.clone()) {
                    if let Some(err) = parsed.error {
                        return Err(format!("MCP error ({}): {}", err.code, err.message));
                    }
                    return Ok(parsed.result.unwrap_or(serde_json::Value::Null));
                }

                if let Some(result) = body.get("result") {
                    return Ok(result.clone());
                }

                Ok(body)
            }
        }
    }

    async fn send_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), String> {
        match &self.transport {
            McpTransport::Stdio(inner) => {
                let mut inner = inner.lock().await;
                let notification = JsonRpcRequest {
                    jsonrpc: "2.0".to_string(),
                    id: None,
                    method: method.to_string(),
                    params,
                };
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
            McpTransport::StreamableHttp(inner) => {
                let inner = inner.lock().await;
                let notification = JsonRpcRequest {
                    jsonrpc: "2.0".to_string(),
                    id: None,
                    method: method.to_string(),
                    params,
                };

                let mut req = inner.client.post(&inner.endpoint).json(&notification);
                for (k, v) in &inner.headers {
                    req = req.header(k, v);
                }

                let response = req
                    .send()
                    .await
                    .map_err(|e| format!("HTTP notification failed: {e}"))?;
                if response.status().is_success() {
                    Ok(())
                } else {
                    Err(format!(
                        "HTTP notification failed with status {}",
                        response.status()
                    ))
                }
            }
        }
    }

    async fn initialize(&self) -> Result<String, String> {
        let params = serde_json::json!({
            "protocolVersion": self.requested_protocol,
            "capabilities": {},
            "clientInfo": {
                "name": "microclaw",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let result = self.send_request("initialize", Some(params)).await?;
        let negotiated = result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.requested_protocol)
            .to_string();

        if negotiated != self.requested_protocol {
            info!(
                "MCP server '{}' negotiated protocol {} (requested {})",
                self.name, negotiated, self.requested_protocol
            );
        }

        self.send_notification("notifications/initialized", None)
            .await?;

        Ok(negotiated)
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
            let input_schema = tool
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));

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

        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

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

    #[allow(dead_code)]
    pub fn protocol_version(&self) -> &str {
        &self.negotiated_protocol
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
                McpServer::connect(
                    name,
                    server_config,
                    config.default_protocol_version.as_deref(),
                ),
            )
            .await
            {
                Ok(Ok(server)) => {
                    info!(
                        "MCP server '{name}' connected ({} tools, protocol {})",
                        server.tools().len(),
                        server.protocol_version()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_config_defaults() {
        let json = r#"{
          "mcpServers": {
            "demo": {
              "command": "npx",
              "args": ["-y", "@modelcontextprotocol/server-filesystem", "."]
            }
          }
        }"#;

        let cfg: McpConfig = serde_json::from_str(json).unwrap();
        let server = cfg.mcp_servers.get("demo").unwrap();
        assert_eq!(server.transport, "stdio");
        assert!(server.protocol_version.is_none());
    }

    #[test]
    fn test_mcp_http_config_parse() {
        let json = r#"{
          "default_protocol_version": "2025-11-05",
          "mcpServers": {
            "remote": {
              "transport": "streamable_http",
              "endpoint": "http://127.0.0.1:8080/mcp",
              "headers": {"Authorization": "Bearer test"}
            }
          }
        }"#;

        let cfg: McpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.default_protocol_version.unwrap(), "2025-11-05");
        let remote = cfg.mcp_servers.get("remote").unwrap();
        assert_eq!(remote.transport, "streamable_http");
        assert_eq!(remote.endpoint, "http://127.0.0.1:8080/mcp");
    }
}
