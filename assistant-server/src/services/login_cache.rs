use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct LoginCacheEntry {
    pub cookie_file: PathBuf,
    pub user_agent: String,
    pub token: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct LoginCache {
    cache: Arc<RwLock<HashMap<String, LoginCacheEntry>>>,
    ttl_minutes: i64,
}

impl LoginCache {
    pub fn new(ttl_minutes: i64) -> Self {
        LoginCache {
            cache: Arc::new(RwLock::new(HashMap::new())),
            ttl_minutes,
        }
    }

    pub async fn get(&self, username: &str) -> Option<LoginCacheEntry> {
        let cache = self.cache.read().await;
        match cache.get(username) {
            Some(entry) => {
                if self.is_valid(entry) {
                    Some(entry.clone())
                } else {
                    None
                }
            }
            None => None,
        }
    }

    pub async fn set(&self, username: &str, cookie_file: PathBuf, user_agent: String, token: String) {
        let mut cache = self.cache.write().await;
        cache.insert(username.to_string(), LoginCacheEntry {
            cookie_file,
            user_agent,
            token,
            created_at: Utc::now(),
        });
    }

    pub async fn invalidate(&self, username: &str) {
        let mut cache = self.cache.write().await;
        if let Some(entry) = cache.remove(username) {
            let _ = std::fs::remove_file(&entry.cookie_file);
        }
    }

    pub async fn cleanup_expired(&self) {
        let mut cache = self.cache.write().await;
        let expired_keys: Vec<String> = cache
            .iter()
            .filter(|(_, entry)| !self.is_valid(entry))
            .map(|(k, _)| k.clone())
            .collect();
        
        for key in expired_keys {
            if let Some(entry) = cache.remove(&key) {
                let _ = std::fs::remove_file(&entry.cookie_file);
            }
        }
    }

    fn is_valid(&self, entry: &LoginCacheEntry) -> bool {
        let now = Utc::now();
        let age = now - entry.created_at;
        age.num_minutes() < self.ttl_minutes
    }

    pub async fn get_stats(&self) -> (usize, usize) {
        let cache = self.cache.read().await;
        let total = cache.len();
        let valid = cache.iter().filter(|(_, entry)| self.is_valid(entry)).count();
        (total, valid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_cache_ttl() {
        let cache = LoginCache::new(1); // 1分钟TTL
        
        cache.set("test_user", PathBuf::from("/tmp/test_cookie.txt"), "test_ua".to_string(), "test_token".to_string()).await;
        
        assert!(cache.get("test_user").await.is_some());
        
        tokio::time::sleep(Duration::from_secs(61)).await;
        
        assert!(cache.get("test_user").await.is_none());
    }
}
