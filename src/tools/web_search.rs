use async_trait::async_trait;
use serde_json::json;

use super::{schema_object, Tool, ToolResult};
use crate::claude::ToolDefinition;

pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_search".into(),
            description: "Search the web using DuckDuckGo. Returns titles, URLs, and snippets."
                .into(),
            input_schema: schema_object(
                json!({
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    }
                }),
                &["query"],
            ),
        }
    }

    async fn execute(&self, input: serde_json::Value) -> ToolResult {
        let query = match input.get("query").and_then(|v| v.as_str()) {
            Some(q) => q,
            None => return ToolResult::error("Missing required parameter: query".into()),
        };

        match search_ddg(query).await {
            Ok(results) => {
                if results.is_empty() {
                    ToolResult::success("No results found.".into())
                } else {
                    ToolResult::success(results)
                }
            }
            Err(e) => ToolResult::error(format!("Search failed: {e}")),
        }
    }
}

async fn search_ddg(query: &str) -> Result<String, String> {
    let encoded = urlencoding::encode(query);
    let url = format!("https://html.duckduckgo.com/html/?q={encoded}");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client
        .get(&url)
        .header("User-Agent", "MicroClaw/1.0")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let body = resp.text().await.map_err(|e| e.to_string())?;

    // Parse results using regex
    // DuckDuckGo HTML results have <a class="result__a" href="...">title</a>
    // and <a class="result__snippet">snippet</a>
    let link_re =
        regex::Regex::new(r#"<a[^>]+class="result__a"[^>]+href="([^"]*)"[^>]*>(.*?)</a>"#).unwrap();
    let snippet_re = regex::Regex::new(r#"<a[^>]+class="result__snippet"[^>]*>(.*?)</a>"#).unwrap();
    let tag_re = regex::Regex::new(r"<[^>]+>").unwrap();

    let links: Vec<(String, String)> = link_re
        .captures_iter(&body)
        .map(|cap| {
            let href = cap[1].to_string();
            let title = tag_re.replace_all(&cap[2], "").trim().to_string();
            (href, title)
        })
        .collect();

    let snippets: Vec<String> = snippet_re
        .captures_iter(&body)
        .map(|cap| tag_re.replace_all(&cap[1], "").trim().to_string())
        .collect();

    let mut output = String::new();
    for (i, (href, title)) in links.iter().enumerate().take(8) {
        let snippet = snippets.get(i).map(|s| s.as_str()).unwrap_or("");
        output.push_str(&format!(
            "{}. {}\n   {}\n   {}\n\n",
            i + 1,
            title,
            href,
            snippet
        ));
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_web_search_definition() {
        let tool = WebSearchTool;
        assert_eq!(tool.name(), "web_search");
        let def = tool.definition();
        assert_eq!(def.name, "web_search");
        assert!(def.description.contains("DuckDuckGo"));
        assert!(def.input_schema["properties"]["query"].is_object());
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "query"));
    }

    #[tokio::test]
    async fn test_web_search_missing_query() {
        let tool = WebSearchTool;
        let result = tool.execute(json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("Missing required parameter: query"));
    }

    #[tokio::test]
    async fn test_web_search_null_query() {
        let tool = WebSearchTool;
        let result = tool.execute(json!({"query": null})).await;
        assert!(result.is_error);
        assert!(result.content.contains("Missing required parameter: query"));
    }
}
