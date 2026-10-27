use chrono::{DateTime, Utc};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
};

type Entries = Mutex<HashMap<String, LoginCacheEntry>>;

#[derive(Debug)]
struct CookieFile {
    path: PathBuf,
    io: Mutex<()>,
}
impl Drop for CookieFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Holding a lease pins the cookie file, regardless of cache expiration.
#[derive(Debug, Clone)]
pub struct LoginCacheEntry {
    pub cookie_file: PathBuf,
    pub user_agent: String,
    pub created_at: DateTime<Utc>,
    file: Arc<CookieFile>,
    owner: Weak<Entries>,
    username: String,
}
impl LoginCacheEntry {
    pub fn new(cookie_file: PathBuf, user_agent: String) -> Self {
        Self {
            file: Arc::new(CookieFile {
                path: cookie_file.clone(),
                io: Mutex::new(()),
            }),
            cookie_file,
            user_agent,
            created_at: Utc::now(),
            owner: Weak::new(),
            username: String::new(),
        }
    }
    pub fn lock_cookie(&self) -> std::sync::MutexGuard<'_, ()> {
        self.file.io.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// An old request must never evict a newer session for the same account.
    pub fn invalidate(&self) {
        if let Some(owner) = self.owner.upgrade() {
            let mut entries = owner.lock().unwrap_or_else(|e| e.into_inner());
            if entries
                .get(&self.username)
                .is_some_and(|e| Arc::ptr_eq(&e.file, &self.file))
            {
                entries.remove(&self.username);
            }
        }
    }
    pub fn checked<T>(&self, result: Result<T, String>) -> Result<T, String> {
        if result.is_err() {
            self.invalidate();
        }
        result
    }
}

#[derive(Clone)]
pub struct LoginCache {
    cache: Arc<Entries>,
    login_locks: Arc<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>>,
    ttl_minutes: i64,
}
impl LoginCache {
    pub fn new(ttl_minutes: i64) -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
            login_locks: Arc::new(Mutex::new(HashMap::new())),
            ttl_minutes,
        }
    }
    pub async fn login_lock(&self, username: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.login_locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(username).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(username.into(), Arc::downgrade(&lock));
        lock
    }
    pub async fn get(&self, username: &str) -> Option<LoginCacheEntry> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache
            .get(username)
            .is_some_and(|entry| !self.is_valid(entry))
        {
            cache.remove(username);
        }
        cache.get(username).cloned()
    }
    pub async fn insert(&self, username: &str, mut entry: LoginCacheEntry) -> LoginCacheEntry {
        entry.owner = Arc::downgrade(&self.cache);
        entry.username = username.into();
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(username.into(), entry.clone());
        entry
    }
    pub async fn invalidate(&self, username: &str) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(username);
    }
    pub async fn cleanup_expired(&self) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, entry| self.is_valid(entry));
    }
    fn is_valid(&self, entry: &LoginCacheEntry) -> bool {
        Utc::now() - entry.created_at < chrono::Duration::minutes(self.ttl_minutes)
            && entry.cookie_file.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session() -> LoginCacheEntry {
        let path = std::env::temp_dir().join(format!("pku-test-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&path, "cookies").unwrap();
        LoginCacheEntry::new(path, "test-ua".into())
    }
    #[tokio::test]
    async fn expiry_removes_cache_but_keeps_running_task_alive() {
        let cache = LoginCache::new(60);
        let mut entry = session();
        entry.created_at = Utc::now() - chrono::Duration::minutes(61);
        let active = cache.insert("user", entry).await;
        let path = active.cookie_file.clone();
        assert!(cache.get("user").await.is_none());
        assert!(path.exists());
        drop(active);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn session_is_reusable_before_one_hour_and_not_renewed_by_reads() {
        let cache = LoginCache::new(60);
        let mut entry = session();
        entry.created_at = Utc::now() - chrono::Duration::minutes(59);
        let entry = cache.insert("user", entry).await;
        let read = cache.get("user").await.unwrap();
        assert_eq!(read.created_at, entry.created_at);
        assert_eq!(
            cache.get("user").await.unwrap().created_at,
            entry.created_at
        );
    }
    #[tokio::test]
    async fn old_failure_does_not_remove_new_session() {
        let cache = LoginCache::new(60);
        let old = cache.insert("user", session()).await;
        let new = cache.insert("user", session()).await;
        old.invalidate();
        assert_eq!(
            cache.get("user").await.unwrap().cookie_file,
            new.cookie_file
        );
        new.invalidate();
        assert!(cache.get("user").await.is_none());
    }
    #[tokio::test]
    async fn missing_cookie_and_periodic_cleanup() {
        let cache = LoginCache::new(60);
        let entry = cache.insert("user", session()).await;
        std::fs::remove_file(&entry.cookie_file).unwrap();
        assert!(cache.get("user").await.is_none());
        let mut expired = session();
        let path = expired.cookie_file.clone();
        expired.created_at = Utc::now() - chrono::Duration::hours(2);
        drop(cache.insert("user", expired).await);
        cache.cleanup_expired().await;
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn replacement_releases_old_cookie_and_preserves_new_one() {
        let cache = LoginCache::new(60);
        let old = cache.insert("user", session()).await;
        let old_path = old.cookie_file.clone();
        drop(old);
        let new = cache.insert("user", session()).await;
        assert!(!old_path.exists());
        cache.invalidate("user").await;
        assert!(new.cookie_file.exists());
        let new_path = new.cookie_file.clone();
        drop(new);
        assert!(!new_path.exists());
    }

    #[tokio::test]
    async fn simultaneous_cache_misses_share_one_login_lock() {
        let cache = LoginCache::new(60);
        let first = cache.login_lock("user").await;
        let guard = first.lock().await;
        let second = cache.login_lock("user").await;
        assert!(Arc::ptr_eq(&first, &second));
        assert!(second.try_lock().is_err());
        drop(guard);
        assert!(second.try_lock().is_ok());
    }
}
