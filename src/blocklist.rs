//! Local & online blocklists. Mirrors `lib/blocklist.py`.

use std::collections::HashMap;

use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue, ETAG, IF_NONE_MATCH};
use reqwest::{Client, StatusCode};
use tracing::{info, warn};

#[derive(Debug, Clone, Default)]
pub struct BlocklistData {
    pub users: Vec<String>,
    pub etag: Option<String>,
}

async fn parse_block_list_from_url(
    client: &Client,
    url: &str,
    old: &BlocklistData,
) -> Result<BlocklistData> {
    let mut headers = HeaderMap::new();
    if let Some(etag) = &old.etag {
        if let Ok(value) = HeaderValue::from_str(etag) {
            headers.insert(IF_NONE_MATCH, value);
        }
    }

    let response = client
        .get(url)
        .headers(headers)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;

    if response.status() == StatusCode::NOT_MODIFIED {
        return Ok(old.clone());
    }

    let response = response.error_for_status()?;
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());

    let text = response.text().await?;
    let users = text
        .lines()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect();

    Ok(BlocklistData { users, etag })
}

#[derive(Debug, Default, Clone)]
pub struct OnlineBlocklist {
    blocklist: HashMap<String, BlocklistData>,
    /// Reused across refreshes so the connection pool / TLS sessions survive
    /// — a fresh `Client::new()` per refresh would discard them. (Clones are
    /// cheap: `reqwest::Client` is `Arc`-backed internally.)
    client: Client,
}

impl OnlineBlocklist {
    pub async fn new(urls: Vec<String>) -> Self {
        let mut me = Self {
            blocklist: urls
                .into_iter()
                .map(|u| (u, BlocklistData::default()))
                .collect(),
            client: Client::new(),
        };
        me.refresh().await;
        me
    }

    pub async fn refresh(&mut self) {
        info!(count = self.blocklist.len(), "refreshing online blocklists");
        let urls: Vec<String> = self.blocklist.keys().cloned().collect();
        for url in urls {
            let old = self.blocklist.get(&url).cloned().unwrap_or_default();
            match parse_block_list_from_url(&self.client, &url, &old).await {
                Ok(data) => {
                    self.blocklist.insert(url, data);
                }
                Err(err) => warn!(%url, %err, "failed to refresh online blocklist"),
            }
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        // Lichess usernames are case-insensitive; a hand-maintained blocklist
        // entry "Alice" must still match the player "alice".
        self.blocklist
            .values()
            .any(|bl| bl.users.iter().any(|u| u.eq_ignore_ascii_case(name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_blocklist_contains_nobody() {
        let bl = OnlineBlocklist::default();
        assert!(!bl.contains("anyone"));
    }

    #[test]
    fn local_lookup_finds_users() {
        let mut bl = OnlineBlocklist::default();
        bl.blocklist.insert(
            "test".into(),
            BlocklistData { users: vec!["alice".into(), "bob".into()], etag: None },
        );
        assert!(bl.contains("alice"));
        assert!(bl.contains("bob"));
        assert!(!bl.contains("carol"));
    }
}
