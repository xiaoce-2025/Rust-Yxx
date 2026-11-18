use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};

type Entries = Mutex<HashMap<String, LoginCacheEntry>>;

#[derive(Debug)]
struct CookieFile {
    path: PathBuf,
    io: Mutex<()>,
    metadata: Option<PathBuf>,
    persistent: AtomicBool,
}
impl Drop for CookieFile {
    fn drop(&mut self) {
        if !self.persistent.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Holding a lease pins the cookie file, until all running requests release it.
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
                metadata: None,
                persistent: AtomicBool::new(false),
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
                if let Some(entry) = entries.remove(&self.username) {
                    entry.retire();
                }
            }
        }
    }
    fn retire(&self) {
        if let Some(path) = &self.file.metadata {
            let _ = std::fs::remove_file(path);
        }
        self.file.persistent.store(false, Ordering::Relaxed);
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
    directory: Option<PathBuf>,
}
#[derive(Serialize, Deserialize)]
struct SavedSession {
    username: String,
    user_agent: String,
    created_at: DateTime<Utc>,
}
impl LoginCache {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
            login_locks: Arc::new(Mutex::new(HashMap::new())),
            directory: None,
        }
    }
    /// Cookie files stay live on disk, so curl updates survive a process restart too.
    pub fn persistent(directory: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut cache = Self::new();
        cache.directory = Some(directory.clone());
        for item in std::fs::read_dir(&directory)? {
            let path = item?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            // Only our generated UUID names can identify a cookie jar.
            if path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
                .is_none()
            {
                continue;
            }
            let saved = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<SavedSession>(&bytes).ok());
            let cookie = path.with_extension("cookies");
            if let Some(saved) = saved {
                let mut entry = LoginCacheEntry::new(cookie.clone(), saved.user_agent);
                entry.created_at = saved.created_at;
                if cache.is_valid(&entry) {
                    let file = Arc::get_mut(&mut entry.file).unwrap();
                    file.metadata = Some(path);
                    file.persistent.store(true, Ordering::Relaxed);
                    entry.owner = Arc::downgrade(&cache.cache);
                    entry.username = saved.username.clone();
                    let mut entries = cache.cache.lock().unwrap_or_else(|e| e.into_inner());
                    if entries
                        .get(&saved.username)
                        .is_some_and(|old| old.created_at > entry.created_at)
                    {
                        entry.retire();
                    } else if let Some(old) = entries.insert(saved.username, entry) {
                        old.retire();
                    }
                    continue;
                }
            }
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(cookie);
        }
        // A crash may leave a copied jar without committed metadata, or an old leased jar.
        for item in std::fs::read_dir(&directory)? {
            let path = item?.path();
            if path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
                .is_none()
            {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) == Some("tmp")
                || (path.extension().and_then(|s| s.to_str()) == Some("cookies")
                    && !path.with_extension("json").exists())
            {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(cache)
    }

    fn persist(&self, username: &str, entry: &LoginCacheEntry) -> std::io::Result<LoginCacheEntry> {
        let directory = self.directory.as_ref().expect("persistent cache directory");
        let path = directory
            .join(uuid::Uuid::new_v4().to_string())
            .with_extension("cookies");
        let mut saved = LoginCacheEntry::new(path.clone(), entry.user_agent.clone());
        saved.created_at = entry.created_at;
        let _guard = entry.lock_cookie();
        std::fs::copy(&entry.cookie_file, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        let metadata = path.with_extension("json");
        let temporary = path.with_extension("tmp");
        let bytes = serde_json::to_vec(&SavedSession {
            username: username.into(),
            user_agent: entry.user_agent.clone(),
            created_at: entry.created_at,
        })?;
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(&temporary, &metadata)?;
        let file = Arc::get_mut(&mut saved.file).unwrap();
        file.metadata = Some(metadata);
        file.persistent.store(true, Ordering::Relaxed);
        Ok(saved)
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
            if let Some(entry) = cache.remove(username) {
                entry.retire();
            }
        }
        cache.get(username).cloned()
    }
    pub async fn insert(&self, username: &str, mut entry: LoginCacheEntry) -> LoginCacheEntry {
        if self.directory.is_some() {
            match self.persist(username, &entry) {
                Ok(saved) => entry = saved,
                Err(error) => {
                    tracing::error!(%error, "教学网登录缓存持久化失败，本次会话仅在内存中可用")
                }
            }
        }
        entry.owner = Arc::downgrade(&self.cache);
        entry.username = username.into();
        if let Some(old) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(username.into(), entry.clone())
        {
            old.retire();
        }
        entry
    }
    pub async fn invalidate(&self, username: &str) {
        if let Some(entry) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(username)
        {
            entry.retire();
        }
    }
    pub async fn cleanup_missing_cookies(&self) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, entry| {
                let valid = self.is_valid(entry);
                if !valid {
                    entry.retire();
                }
                valid
            });
    }
    fn is_valid(&self, entry: &LoginCacheEntry) -> bool {
        // The teaching server decides when cookies expire; age alone never evicts a session.
        entry.cookie_file.is_file()
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
    async fn restart_restores_old_cookie_updates_user_agent_and_login_time() {
        let dir = tempdir::TempDir::new("persistent-login").unwrap();
        let cache = LoginCache::persistent(dir.path().into()).unwrap();
        let mut original = session();
        original.created_at = Utc::now() - chrono::Duration::days(365);
        let entry = cache.insert("user", original).await;
        let created = entry.created_at;
        let cookie = entry.cookie_file.clone();
        std::fs::write(&cookie, "refreshed cookies").unwrap();
        drop(entry);
        drop(cache);
        assert!(cookie.is_file());
        let restored = LoginCache::persistent(dir.path().into()).unwrap();
        let entry = restored.get("user").await.unwrap();
        assert_eq!(entry.created_at, created);
        assert_eq!(entry.user_agent, "test-ua");
        assert_eq!(
            std::fs::read_to_string(&entry.cookie_file).unwrap(),
            "refreshed cookies"
        );
        restored.invalidate("user").await;
        assert!(cookie.is_file()); // running requests keep their lease
        drop(entry);
        assert!(!cookie.exists());
        drop(restored);
        assert!(LoginCache::persistent(dir.path().into())
            .unwrap()
            .get("user")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn persistent_old_sessions_survive_and_stale_failures_do_not_evict_replacements() {
        let dir = tempdir::TempDir::new("persistent-login").unwrap();
        let cache = LoginCache::persistent(dir.path().into()).unwrap();
        let old = cache.insert("user", session()).await;
        let new = cache.insert("user", session()).await;
        old.invalidate();
        let cookie = new.cookie_file.clone();
        drop(old);
        drop(new);
        let mut aged = session();
        aged.created_at = Utc::now() - chrono::Duration::days(365);
        drop(cache.insert("aged", aged).await);
        drop(cache);
        let restored = LoginCache::persistent(dir.path().into()).unwrap();
        assert_eq!(restored.get("user").await.unwrap().cookie_file, cookie);
        assert!(restored.get("aged").await.is_some());
        restored.get("user").await.unwrap().invalidate();
        drop(restored);
        assert!(LoginCache::persistent(dir.path().into())
            .unwrap()
            .get("user")
            .await
            .is_none());
    }
    #[tokio::test]
    async fn invalidation_removes_cache_but_keeps_running_task_alive() {
        let cache = LoginCache::new();
        let mut entry = session();
        entry.created_at = Utc::now() - chrono::Duration::days(365);
        let active = cache.insert("user", entry).await;
        let path = active.cookie_file.clone();
        active.invalidate();
        assert!(cache.get("user").await.is_none());
        assert!(path.exists());
        drop(active);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn old_session_survives_reads_and_periodic_cleanup() {
        let cache = LoginCache::new();
        let mut entry = session();
        entry.created_at = Utc::now() - chrono::Duration::days(365);
        let entry = cache.insert("user", entry).await;
        cache.cleanup_missing_cookies().await;
        let read = cache.get("user").await.unwrap();
        assert_eq!(read.created_at, entry.created_at);
        assert_eq!(
            cache.get("user").await.unwrap().created_at,
            entry.created_at
        );
    }
    #[tokio::test]
    async fn old_failure_does_not_remove_new_session() {
        let cache = LoginCache::new();
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
        let cache = LoginCache::new();
        let entry = cache.insert("user", session()).await;
        std::fs::remove_file(&entry.cookie_file).unwrap();
        assert!(cache.get("user").await.is_none());
        let missing = cache.insert("missing", session()).await;
        std::fs::remove_file(&missing.cookie_file).unwrap();
        cache.cleanup_missing_cookies().await;
        assert!(cache.get("missing").await.is_none());
        assert!(cache.cache.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn replacement_releases_old_cookie_and_preserves_new_one() {
        let cache = LoginCache::new();
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
        let cache = LoginCache::new();
        let first = cache.login_lock("user").await;
        let guard = first.lock().await;
        let second = cache.login_lock("user").await;
        assert!(Arc::ptr_eq(&first, &second));
        assert!(second.try_lock().is_err());
        drop(guard);
        assert!(second.try_lock().is_ok());
    }
}
