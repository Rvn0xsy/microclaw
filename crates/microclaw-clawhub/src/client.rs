use crate::types::*;
use microclaw_core::error::MicroClawError;

pub struct ClawHubClient {
    base_url: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl ClawHubClient {
    pub fn new(base_url: &str, token: Option<String>) -> Self {
        Self {
            base_url: base_url.to_string(),
            token,
            client: reqwest::Client::new(),
        }
    }

    /// Search skills by query
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        _sort: &str,
    ) -> Result<Vec<SearchResult>, MicroClawError> {
        // Use the dedicated search endpoint that actually filters by query
        let url = format!(
            "{}/api/v1/search?q={}&limit={}",
            self.base_url, query, limit
        );
        let mut req = self.client.get(&url);
        if let Some(ref token) = self.token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| MicroClawError::Config(format!("ClawHub request failed: {}", e)))?;
        let search_response: ApiSearchResponse = resp.json().await.map_err(|e| {
            MicroClawError::Config(format!("Failed to parse search results: {}", e))
        })?;
        // Convert API response items to internal SearchResult type
        Ok(search_response
            .results
            .into_iter()
            .take(limit)
            .map(SearchResult::from)
            .collect())
    }

    /// Get skill metadata by slug
    pub async fn get_skill(&self, slug: &str) -> Result<SkillMeta, MicroClawError> {
        let url = format!("{}/api/v1/skills/{}", self.base_url, slug);
        let mut req = self.client.get(&url);
        if let Some(ref token) = self.token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| MicroClawError::Config(format!("ClawHub request failed: {}", e)))?;
        let get_response: GetSkillResponse = resp.json().await.map_err(|e| {
            MicroClawError::Config(format!("Failed to parse skill metadata: {}", e))
        })?;
        // Convert API response to internal SkillMeta type
        Ok(SkillMeta::from(get_response))
    }

    /// Download skill as ZIP bytes
    pub async fn download_skill(
        &self,
        slug: &str,
        version: &str,
    ) -> Result<Vec<u8>, MicroClawError> {
        // Use Convex-backed download endpoint (different from REST API)
        // Format: https://{random}.convex.site/api/v1/download?slug={slug}&version={version}
        // The subdomain appears to be consistent for all downloads
        let url = format!(
            "https://wry-manatee-359.convex.site/api/v1/download?slug={}&version={}",
            slug, version
        );
        let mut req = self.client.get(&url);
        if let Some(ref token) = self.token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| MicroClawError::Config(format!("ClawHub download failed: {}", e)))?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| MicroClawError::Config(format!("Failed to read download: {}", e)))?;
        Ok(bytes.to_vec())
    }

    /// List versions for a skill
    pub async fn get_versions(&self, slug: &str) -> Result<Vec<SkillVersion>, MicroClawError> {
        let url = format!("{}/api/v1/skills/{}/versions", self.base_url, slug);
        let mut req = self.client.get(&url);
        if let Some(ref token) = self.token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| MicroClawError::Config(format!("ClawHub request failed: {}", e)))?;
        let versions: Vec<SkillVersion> = resp
            .json()
            .await
            .map_err(|e| MicroClawError::Config(format!("Failed to parse versions: {}", e)))?;
        Ok(versions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_construction() {
        let client = ClawHubClient::new("https://clawhub.ai", None);
        assert_eq!(client.base_url, "https://clawhub.ai");
        assert!(client.token.is_none());
    }

    #[test]
    fn test_client_with_token() {
        let client = ClawHubClient::new("https://clawhub.ai", Some("test-token".into()));
        assert!(client.token.is_some());
    }
}
