use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use serde::de::DeserializeOwned;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Sqlite, SqlitePool, Transaction};
use tokio::sync::RwLock;
use uuid::Uuid;




pub fn default_processing_priority() -> u32 { 5 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileUser {
    #[serde(default = "default_processing_priority")]
    pub processing_priority: u32,
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub password_hash: String,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub storage_quota: i32,
    pub processing_quota: i32,
    #[serde(default)]
    pub iaaa_encrypted_password: Option<String>,
    #[serde(default)]
    pub iaaa_public_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileUserSession {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub user_agent: Option<String>,
    pub ip_address: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiMode {
    Server,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileUserConfig {
    #[serde(default)]
    pub ai_mode: Option<AiMode>,
    pub user_id: Uuid,
    pub openai_api_key: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub default_model: String,
    pub theme: String,
    pub settings: serde_json::Value,
    // 用户自定义API配置（会覆盖全局配置）
    #[serde(default)]
    pub aliyun_model: Option<String>,
    #[serde(default)]
    pub aliyun_api_key: Option<String>,
    #[serde(default)]
    pub deepseek_model: Option<String>,
    #[serde(default)]
    pub deepseek_api_key: Option<String>,
    #[serde(default)]
    pub mimo_model: Option<String>,
    #[serde(default)]
    pub mimo_api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileConversation {
    pub id: Uuid,
    pub user_id: Uuid,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileConversationMessage {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub role: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileUserWeChatBinding {
    pub id: Uuid,
    pub user_id: Uuid,
    pub openid: String,
    pub unionid: Option<String>,
    pub nickname: Option<String>,
    pub avatar_url: Option<String>,
    pub bound_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileUserWeChatChannel {
    pub user_id: Uuid,
    pub bot_token: Option<String>,
    pub account_id: Option<String>,
    pub login_user_id: Option<String>,
    pub active_user_id: Option<String>,
    pub get_updates_cursor: Option<String>,
    pub listener_started: bool,
    pub listener_verify_text: Option<String>,
    pub listener_verify_status: Option<String>,
    pub last_user_message_time_ms: Option<i64>,
    pub token_expiry_reminder_sent: bool,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileWeChatOAuthState {
    pub state: String,
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationWhitelist {
    pub allowed_emails: Vec<String>,
}

type StoreError = Box<dyn std::error::Error + Send + Sync>;

pub struct FileStore {
    data_dir: PathBuf,
    db: SqlitePool,
    users: RwLock<HashMap<Uuid, FileUser>>,
    sessions: RwLock<HashMap<Uuid, FileUserSession>>,
    configs: RwLock<HashMap<Uuid, FileUserConfig>>,
    conversations: RwLock<HashMap<Uuid, FileConversation>>,
    messages: RwLock<Vec<FileConversationMessage>>,
    wechat_bindings: RwLock<Vec<FileUserWeChatBinding>>,
    wechat_channels: RwLock<HashMap<Uuid, FileUserWeChatChannel>>,
    oauth_states: RwLock<Vec<FileWeChatOAuthState>>,
    pkuvideo_jobs: RwLock<HashMap<Uuid, crate::services::pkuvideo::PkuVideoJob>>,
    pkuvideo_fw_jobs: RwLock<HashMap<Uuid, crate::services::pkuvideo_fw::PkuVideoFwJob>>,
    oidc_identities: RwLock<HashMap<String, Uuid>>,
    whitelist: RwLock<RegistrationWhitelist>,
}

async fn import_collection<T: DeserializeOwned + Default + Serialize>(
    tx: &mut Transaction<'_, Sqlite>,
    data_dir: &std::path::Path,
    name: &str,
) -> Result<T, StoreError> {
    let stored: Option<String> =
        sqlx::query_scalar("SELECT payload FROM collections WHERE name = ?")
            .bind(name)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(payload) = stored {
        return Ok(serde_json::from_str(&payload)?);
    }

    let path = data_dir.join(format!("{name}.json"));
    let value = match tokio::fs::read_to_string(&path).await {
        Ok(payload) if name == "wechat_channels" => {
            // Older versions serialized channels as an array.
            let channels: Vec<FileUserWeChatChannel> = serde_json::from_str(&payload)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            let map: HashMap<_, _> = channels.into_iter().map(|ch| (ch.user_id, ch)).collect();
            serde_json::from_value(serde_json::to_value(map)?)?
        }
        Ok(payload) => serde_json::from_str(&payload)
            .map_err(|error| format!("{}: {error}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => T::default(),
        Err(error) => return Err(error.into()),
    };
    write_collection(&mut **tx, name, &serde_json::to_string(&value)?).await?;
    Ok(value)
}

async fn write_collection(
    connection: &mut sqlx::SqliteConnection,
    name: &str,
    payload: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO collections (name, payload) VALUES (?, ?)
         ON CONFLICT(name) DO UPDATE SET payload = excluded.payload",
    )
    .bind(name)
    .bind(payload)
    .execute(connection)
    .await?;
    Ok(())
}

impl FileStore {
    pub async fn admin_users(&self) -> Vec<FileUser> {
        self.users.read().await.values().cloned().collect()
    }

    pub async fn admin_update_user(&self, id: Uuid, active: bool, storage: i32, processing: i32, priority: u32) -> Result<(), crate::error::ApiError> {
        use crate::error::ApiError;
        let mut users = self.users.write().await;
        let mut updated = users.clone();
        let user = updated.get_mut(&id).ok_or(ApiError::NotFound)?;
        user.is_active = active;
        user.storage_quota = storage;
        user.processing_quota = processing;
        user.processing_priority = priority;
        user.updated_at = Utc::now();
        self.save_collection("users", &updated).await.map_err(|e| ApiError::Internal(e.to_string()))?;
        *users = updated;
        Ok(())
    }

    pub async fn admin_revoke_sessions(&self, user_id: Uuid) -> Result<usize, StoreError> {
        let mut sessions = self.sessions.write().await;
        let mut updated = sessions.clone();
        updated.retain(|_, s| s.user_id != user_id);
        let count = sessions.len() - updated.len();
        self.save_collection("sessions", &updated).await?;
        *sessions = updated;
        Ok(count)
    }

    pub async fn admin_session_counts(&self) -> HashMap<Uuid, usize> {
        let mut counts = HashMap::new();
        for session in self.sessions.read().await.values().filter(|s| s.expires_at > Utc::now()) {
            *counts.entry(session.user_id).or_insert(0) += 1;
        }
        counts
    }

    pub async fn admin_whitelist(&self) -> Vec<String> {
        self.whitelist.read().await.allowed_emails.clone()
    }

    pub async fn admin_save_whitelist(&self, emails: Vec<String>) -> Result<(), StoreError> {
        let mut whitelist = self.whitelist.write().await;
        let updated = RegistrationWhitelist { allowed_emails: emails };
        let path = self.data_dir.join("whitelist.json");
        let temporary = self.data_dir.join("whitelist.json.tmp");
        tokio::fs::write(&temporary, serde_json::to_vec_pretty(&updated)?).await?;
        tokio::fs::rename(&temporary, &path).await?;
        *whitelist = updated;
        Ok(())
    }

    pub async fn new(data_dir: PathBuf) -> Result<Self, StoreError> {
        tokio::fs::create_dir_all(&data_dir).await?;
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(data_dir.join("assistant.sqlite3"))
                    .create_if_missing(true),
            )
            .await?;
        let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS collections (
                name TEXT PRIMARY KEY NOT NULL,
                payload TEXT NOT NULL CHECK (json_valid(payload))
            )",
        )
        .execute(&mut *tx)
        .await?;
        let store = Self {
            users: RwLock::new(import_collection(&mut tx, &data_dir, "users").await?),
            sessions: RwLock::new(import_collection(&mut tx, &data_dir, "sessions").await?),
            configs: RwLock::new(import_collection(&mut tx, &data_dir, "configs").await?),
            conversations: RwLock::new(import_collection(&mut tx, &data_dir, "conversations").await?),
            messages: RwLock::new(import_collection(&mut tx, &data_dir, "messages").await?),
            wechat_bindings: RwLock::new(import_collection(&mut tx, &data_dir, "wechat_bindings").await?),
            wechat_channels: RwLock::new(import_collection(&mut tx, &data_dir, "wechat_channels").await?),
            oauth_states: RwLock::new(import_collection(&mut tx, &data_dir, "oauth_states").await?),
            pkuvideo_jobs: RwLock::new(import_collection(&mut tx, &data_dir, "pkuvideo_jobs").await?),
            pkuvideo_fw_jobs: RwLock::new(import_collection(&mut tx, &data_dir, "pkuvideo_fw_jobs").await?),
            oidc_identities: RwLock::new(import_collection(&mut tx, &data_dir, "oidc_identities").await?),
            whitelist: RwLock::new(RegistrationWhitelist { allowed_emails: Vec::new() }),
            data_dir,
            db: db.clone(),
        };
        tx.commit().await?;
        store.load_whitelist().await?;
        Ok(store)
    }

    async fn save_collection<T: Serialize>(&self, name: &str, value: &T) -> Result<(), StoreError> {
        let payload = serde_json::to_string(value)?;
        let mut connection = self.db.acquire().await?;
        write_collection(&mut connection, name, &payload).await?;
        Ok(())
    }

    fn sidecar_key(path: &std::path::Path) -> String {
        let parts: Vec<_> = path.components().map(|part| part.as_os_str().to_string_lossy()).collect();
        format!("sidecar/{}", parts.join("/"))
    }

    pub async fn stored_sidecar(&self, path: &std::path::Path) -> Result<Option<String>, String> {
        sqlx::query_scalar("SELECT payload FROM collections WHERE name = ?")
            .bind(Self::sidecar_key(path))
            .fetch_optional(&self.db)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn save_sidecar(&self, path: &std::path::Path, payload: &str) -> Result<(), String> {
        let mut connection = self.db.acquire().await.map_err(|error| error.to_string())?;
        write_collection(&mut connection, &Self::sidecar_key(path), payload)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn sidecar_paths_under(&self, directory: &std::path::Path) -> Result<Vec<String>, String> {
        let prefix = format!("{}/", Self::sidecar_key(directory));
        sqlx::query_scalar("SELECT substr(name, length(?) + 1) FROM collections WHERE substr(name, 1, length(?)) = ?")
            .bind(&prefix)
            .bind(&prefix)
            .bind(&prefix)
            .fetch_all(&self.db)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn delete_sidecar(&self, path: &std::path::Path) -> Result<(), String> {
        sqlx::query("DELETE FROM collections WHERE name = ?")
            .bind(Self::sidecar_key(path))
            .execute(&self.db)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub async fn load_sidecar<T: DeserializeOwned>(
        &self,
        path: &std::path::Path,
    ) -> Result<Option<T>, String> {
        if let Some(payload) = self.stored_sidecar(path).await? {
            return serde_json::from_str(&payload).map_err(|error| error.to_string());
        }

        let payload = match tokio::fs::read_to_string(path).await {
            Ok(payload) => payload,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        serde_json::from_str::<T>(&payload)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        // A concurrent refresh may already have replaced the legacy version.
        let payload: String = sqlx::query_scalar(
            "INSERT INTO collections (name, payload) VALUES (?, ?)
             ON CONFLICT(name) DO UPDATE SET payload = collections.payload
             RETURNING payload",
        )
        .bind(Self::sidecar_key(path))
        .bind(payload)
        .fetch_one(&self.db)
        .await
        .map_err(|error| error.to_string())?;
        serde_json::from_str(&payload).map_err(|error| error.to_string())
    }

    async fn update_collection<T: Serialize + Clone>(
        &self,
        name: &str,
        collection: &RwLock<T>,
        update: impl FnOnce(&mut T),
    ) -> Result<(), StoreError> {
        let mut current = collection.write().await;
        let mut next = current.clone();
        update(&mut next);
        self.save_collection(name, &next).await?;
        *current = next;
        Ok(())
    }

    pub async fn get_user(&self, user_id: Uuid) -> Option<FileUser> {
        self.users.read().await.get(&user_id).cloned()
    }

    pub async fn get_user_by_email(&self, email: &str) -> Option<FileUser> {
        self.users
            .read()
            .await
            .values()
            .find(|u| u.email == email)
            .cloned()
    }

    pub async fn get_user_by_username(&self, username: &str) -> Option<FileUser> {
        self.users
            .read()
            .await
            .values()
            .find(|u| u.username == username)
            .cloned()
    }

    pub async fn insert_user(&self, user: FileUser) -> Result<(), StoreError> {
        let mut users = self.users.write().await;
        if users.values().any(|u| u.email == user.email) {
            return Err("邮箱已存在".into());
        }
        let mut updated = users.clone();
        updated.insert(user.id, user);
        self.save_collection("users", &updated).await?;
        *users = updated;
        Ok(())
    }

    /// A verified OIDC identity still needs this application's own whitelist on every login.
    pub async fn resolve_oidc_user(&self, issuer: &str, subject: &str, email: &str, username: &str) -> Result<FileUser, crate::error::ApiError> {
        use crate::error::ApiError;
        if !self.is_email_in_whitelist(email).await {
            return Err(ApiError::Forbidden("统一认证已通过，但该邮箱不在小助手白名单中".into()));
        }
        let mut users = self.users.write().await;
        let mut links = self.oidc_identities.write().await;
        let mut new_user = false;
        let identity = format!("{}\0{}", issuer.trim_end_matches('/'), subject);
        let user = if let Some(id) = links.get(&identity) {
            let user = users.get(id).ok_or_else(|| ApiError::Forbidden("关联账户不存在，请联系管理员".into()))?;
            if user.email != email { return Err(ApiError::Forbidden("统一账户邮箱已变更，请联系管理员重新关联".into())); }
            user.clone()
        } else if let Some(user) = users.values().find(|user| user.email == email) {
            let prefix = format!("{}\0", issuer.trim_end_matches('/'));
            if links.iter().any(|(key, id)| key.starts_with(&prefix) && id == &user.id) {
                return Err(ApiError::Forbidden("此邮箱已关联其他统一认证账户，请联系管理员".into()));
            }
            user.clone()
        } else {
            let id = Uuid::new_v4();
            let now = Utc::now();
            let user = FileUser {
                id, username: if users.values().any(|u|u.username == username) {format!("{username}-{id}")} else {username.to_string()},
                email: email.to_string(), password_hash: crate::auth::hash_password(&Uuid::new_v4().to_string())?,
                is_active: true, created_at: now, updated_at: now, storage_quota: 3, processing_quota: 1,
                processing_priority: 5, iaaa_encrypted_password: None, iaaa_public_key: None,
            };
            new_user = true;
            user
        };
        if !user.is_active {
            return Err(ApiError::Forbidden("小助手账户已停用".into()));
        }
        if links.contains_key(&identity) {
            return Ok(user);
        }
        let mut updated_links = links.clone();
        updated_links.insert(identity, user.id);
        let mut updated_users = users.clone();
        if new_user {
            updated_users.insert(user.id, user.clone());
        }
        let persist = async {
            let mut tx = self.db.begin().await?;
            if new_user {
                write_collection(&mut tx, "users", &serde_json::to_string(&updated_users)?).await?;
            }
            write_collection(&mut tx, "oidc_identities", &serde_json::to_string(&updated_links)?).await?;
            tx.commit().await?;
            Ok::<_, StoreError>(())
        };
        persist.await.map_err(|error| {
            tracing::error!(%error, "保存统一认证账户失败");
            ApiError::Internal("保存统一认证账户失败".into())
        })?;
        *users = updated_users;
        *links = updated_links;
        Ok(user)
    }

    /// Resolve a verified IAAA identity atomically. Only new accounts require the whitelist.
    pub async fn resolve_iaaa_user(&self, mut candidate: FileUser) -> Result<Option<FileUser>, StoreError> {
        let mut users = self.users.write().await;
        if let Some(user) = users.values().find(|u| u.email == candidate.email) {
            tracing::info!(user_id=%user.id, is_active=user.is_active, "IAAA identity resolved to existing account");
            return Ok(Some(user.clone()));
        }
        if !self.is_email_in_whitelist(&candidate.email).await {
            tracing::warn!(email=%candidate.email, "IAAA registration denied: email is not in the whitelist");
            return Ok(None);
        }
        if users.values().any(|u| u.username == candidate.username) {
            candidate.username = format!("{}-{}", candidate.username, candidate.id);
        }
        let mut updated = users.clone();
        updated.insert(candidate.id, candidate.clone());
        self.save_collection("users", &updated).await?;
        *users = updated;
        tracing::info!(user_id=%candidate.id, "IAAA whitelist check passed; new account registered");
        Ok(Some(candidate))
    }

    pub async fn decrease_processing_quota(&self, user_id: Uuid) -> Result<(), StoreError> {
        self.update_collection("users", &self.users, |users| {
            if let Some(user) = users.get_mut(&user_id) {
                if user.processing_quota > 0 {
                    user.processing_quota -= 1;
                    user.updated_at = chrono::Utc::now();
                }
            }
        }).await
    }

    pub async fn increase_storage_quota(&self, user_id: Uuid, amount: i32) -> Result<(), StoreError> {
        self.update_collection("users", &self.users, |users| {
            if let Some(user) = users.get_mut(&user_id) {
                user.storage_quota += amount;
                user.updated_at = chrono::Utc::now();
            }
        }).await
    }

    pub async fn decrease_storage_quota(&self, user_id: Uuid, amount: i32) -> Result<(), StoreError> {
        self.update_collection("users", &self.users, |users| {
            if let Some(user) = users.get_mut(&user_id) {
                user.storage_quota = std::cmp::max(0, user.storage_quota - amount);
                user.updated_at = chrono::Utc::now();
            }
        }).await
    }

    pub async fn update_iaaa_binding(&self, user_id: Uuid, encrypted_password: String, public_key: String) -> Result<(), StoreError> {
        self.update_collection("users", &self.users, |users| {
            if let Some(user) = users.get_mut(&user_id) {
                user.iaaa_encrypted_password = Some(encrypted_password);
                user.iaaa_public_key = Some(public_key);
                user.updated_at = chrono::Utc::now();
            }
        }).await
    }

    pub async fn remove_iaaa_binding(&self, user_id: Uuid) -> Result<(), StoreError> {
        self.update_collection("users", &self.users, |users| {
            if let Some(user) = users.get_mut(&user_id) {
                user.iaaa_encrypted_password = None;
                user.iaaa_public_key = None;
                user.updated_at = chrono::Utc::now();
            }
        }).await
    }

    // ===== Sessions =====
    pub async fn get_session(&self, session_id: Uuid) -> Option<FileUserSession> {
        let sessions = self.sessions.read().await;
        sessions.get(&session_id).cloned()
    }

    pub async fn insert_session(&self, session: FileUserSession) -> Result<(), StoreError> {
        self.update_collection("sessions", &self.sessions, |records| {
            records.insert(session.session_id, session);
        }).await
    }

    pub async fn delete_session(&self, session_id: Uuid) -> Result<(), StoreError> {
        self.update_collection("sessions", &self.sessions, |records| {
            records.remove(&session_id);
        }).await
    }

    // ===== Configs =====
    pub async fn get_config(&self, user_id: Uuid) -> Option<FileUserConfig> {
        self.configs.read().await.get(&user_id).cloned()
    }

    pub async fn update_config(
        &self,
        user_id: Uuid,
        update: impl FnOnce(Option<FileUserConfig>) -> FileUserConfig,
    ) -> std::io::Result<()> {
        self.update_collection("configs", &self.configs, |configs| {
            let config = update(configs.remove(&user_id));
            configs.insert(user_id, config);
        })
        .await
        .map_err(std::io::Error::other)
    }

    // ===== Conversations =====
    pub async fn get_conversations(&self, user_id: Uuid) -> Vec<FileConversation> {
        let mut convs: Vec<_> = self
            .conversations
            .read()
            .await
            .values()
            .filter(|c| c.user_id == user_id)
            .cloned()
            .collect();
        convs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        convs
    }

    pub async fn get_conversation(&self, conv_id: Uuid) -> Option<FileConversation> {
        self.conversations.read().await.get(&conv_id).cloned()
    }

    pub async fn insert_conversation(&self, conv: FileConversation) -> Result<(), StoreError> {
        self.update_collection("conversations", &self.conversations, |records| {
            records.insert(conv.id, conv);
        }).await
    }

    pub async fn update_conversation(&self, conv: FileConversation) -> Result<(), StoreError> {
        self.update_collection("conversations", &self.conversations, |records| {
            records.insert(conv.id, conv);
        }).await
    }

    // ===== Messages =====
    pub async fn get_messages(&self, conv_id: Uuid) -> Vec<FileConversationMessage> {
        let mut msgs: Vec<_> = self
            .messages
            .read()
            .await
            .iter()
            .filter(|m| m.conversation_id == conv_id)
            .cloned()
            .collect();
        msgs.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        msgs
    }

    pub async fn insert_message(&self, msg: FileConversationMessage) -> Result<(), StoreError> {
        self.update_collection("messages", &self.messages, |records| {
            records.push(msg);
        }).await
    }

    // ===== WeChat Bindings =====
    pub async fn get_wechat_binding(&self, user_id: Uuid) -> Option<FileUserWeChatBinding> {
        let bindings = self.wechat_bindings.read().await;
        bindings
            .iter()
            .filter(|b| b.user_id == user_id)
            .max_by_key(|b| b.updated_at)
            .cloned()
    }

    pub async fn upsert_wechat_binding(&self, binding: FileUserWeChatBinding) -> Result<(), StoreError> {
        self.update_collection("wechat_bindings", &self.wechat_bindings, |records| {
            if let Some(existing) = records.iter_mut().find(|b| b.user_id == binding.user_id && b.openid == binding.openid) {
                *existing = binding;
            } else {
                records.push(binding);
            }
        }).await
    }

    pub async fn delete_wechat_binding(&self, user_id: Uuid) -> Result<(), StoreError> {
        self.update_collection("wechat_bindings", &self.wechat_bindings, |records| {
            records.retain(|b| b.user_id != user_id);
        }).await
    }

    // ===== WeChat Channels =====
    pub async fn get_wechat_channel(&self, user_id: Uuid) -> Option<FileUserWeChatChannel> {
        self.wechat_channels.read().await.get(&user_id).cloned()
    }

    pub async fn upsert_wechat_channel(&self, channel: FileUserWeChatChannel) -> Result<(), StoreError> {
        self.update_collection("wechat_channels", &self.wechat_channels, |records| {
            records.insert(channel.user_id, channel);
        }).await
    }

    pub async fn get_all_wechat_channels(&self) -> Vec<FileUserWeChatChannel> {
        self.wechat_channels.read().await.values().cloned().collect()
    }

    // ===== OAuth States =====
    pub async fn get_oauth_state(&self, state: &str) -> Option<(Uuid, DateTime<Utc>)> {
        self.oauth_states
            .read()
            .await
            .iter()
            .find(|s| s.state == state && s.expires_at > Utc::now())
            .map(|s| (s.user_id, s.expires_at))
    }

    pub async fn insert_oauth_state(&self, state: FileWeChatOAuthState) -> Result<(), StoreError> {
        self.update_collection("oauth_states", &self.oauth_states, |records| {
            records.push(state);
        }).await
    }

    pub async fn delete_oauth_state(&self, state: &str) -> Result<(), StoreError> {
        self.update_collection("oauth_states", &self.oauth_states, |records| {
            records.retain(|s| s.state != state);
        }).await
    }

    // ===== Registration Whitelist =====
    async fn load_whitelist(&self) -> Result<(), StoreError> {
        let path = self.data_dir.join("whitelist.json");
        if path.exists() {
            let data = tokio::fs::read_to_string(&path).await?;
            let whitelist: RegistrationWhitelist = serde_json::from_str(&data)?;
            *self.whitelist.write().await = whitelist;
        } else {
            let default_whitelist = RegistrationWhitelist {
                allowed_emails: Vec::new(),
            };
            let json = serde_json::to_string_pretty(&default_whitelist)?;
            tokio::fs::write(&path, json).await?;
            *self.whitelist.write().await = default_whitelist;
        }
        Ok(())
    }

    pub async fn reload_whitelist(&self) -> Result<(), StoreError> {
        self.load_whitelist().await
    }

    pub async fn is_email_in_whitelist(&self, email: &str) -> bool {
        let whitelist = self.whitelist.read().await;
        whitelist.allowed_emails.iter().any(|e| e == email)
    }

    // ===== PKU Video Jobs =====
    pub async fn get_pkuvideo_job(&self, job_id: Uuid) -> Option<crate::services::pkuvideo::PkuVideoJob> {
        self.pkuvideo_jobs.read().await.get(&job_id).cloned()
    }

    pub async fn get_pkuvideo_jobs_by_user(&self, user_id: Uuid) -> Vec<crate::services::pkuvideo::PkuVideoJob> {
        let jobs = self.pkuvideo_jobs.read().await;
        jobs.values()
            .filter(|job| job.user_id == user_id)
            .cloned()
            .collect()
    }

    pub async fn insert_pkuvideo_job(&self, job: crate::services::pkuvideo::PkuVideoJob) -> Result<(), StoreError> {
        self.update_collection("pkuvideo_jobs", &self.pkuvideo_jobs, |records| {
            records.insert(job.id, job);
        }).await
    }

    pub async fn update_pkuvideo_job(&self, job: crate::services::pkuvideo::PkuVideoJob) -> Result<(), StoreError> {
        self.update_collection("pkuvideo_jobs", &self.pkuvideo_jobs, |records| {
            records.insert(job.id, job);
        }).await
    }

    pub async fn delete_pkuvideo_job(&self, job_id: Uuid) -> Result<(), StoreError> {
        self.update_collection("pkuvideo_jobs", &self.pkuvideo_jobs, |records| {
            records.remove(&job_id);
        }).await
    }

    pub async fn get_all_pkuvideo_jobs(&self) -> HashMap<Uuid, crate::services::pkuvideo::PkuVideoJob> {
        self.pkuvideo_jobs.read().await.clone()
    }

    // ===== PKU Video FW Jobs =====
    pub async fn insert_pkuvideo_fw_job(&self, job: crate::services::pkuvideo_fw::PkuVideoFwJob) -> Result<(), StoreError> {
        self.update_collection("pkuvideo_fw_jobs", &self.pkuvideo_fw_jobs, |records| {
            records.insert(job.id, job);
        }).await
    }

    pub async fn update_pkuvideo_fw_job(&self, job: crate::services::pkuvideo_fw::PkuVideoFwJob) -> Result<(), StoreError> {
        self.update_collection("pkuvideo_fw_jobs", &self.pkuvideo_fw_jobs, |records| {
            records.insert(job.id, job);
        }).await
    }

    pub async fn delete_pkuvideo_fw_job(&self, job_id: Uuid) -> Result<(), StoreError> {
        self.update_collection("pkuvideo_fw_jobs", &self.pkuvideo_fw_jobs, |records| {
            records.remove(&job_id);
        }).await
    }

    pub async fn get_all_pkuvideo_fw_jobs(&self) -> HashMap<Uuid, crate::services::pkuvideo_fw::PkuVideoFwJob> {
        self.pkuvideo_fw_jobs.read().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn legacy_import_rolls_back_and_is_only_applied_once() {
        let dir = tempdir::TempDir::new_in("target", "legacy-store").unwrap();
        let sessions = dir.path().join("sessions.json");
        let configs = dir.path().join("configs.json");
        let id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let now = Utc::now();
        let session = FileUserSession {
            session_id: id,
            user_id,
            expires_at: now + chrono::Duration::hours(1),
            created_at: now,
            user_agent: None,
            ip_address: None,
        };
        tokio::fs::write(&sessions, "{}").await.unwrap();
        tokio::fs::write(&configs, "{broken").await.unwrap();
        assert!(FileStore::new(dir.path().into()).await.is_err());

        let legacy = serde_json::to_string(&HashMap::from([(id, session)])).unwrap();
        tokio::fs::write(&sessions, &legacy).await.unwrap();
        tokio::fs::write(&configs, "{}").await.unwrap();
        let store = FileStore::new(dir.path().into()).await.unwrap();
        assert_eq!(store.get_session(id).await.unwrap().user_id, user_id);
        store.delete_session(id).await.unwrap();
        drop(store);

        let reopened = FileStore::new(dir.path().into()).await.unwrap();
        assert!(reopened.get_session(id).await.is_none());
        assert_eq!(tokio::fs::read_to_string(&sessions).await.unwrap(), legacy);
    }
}
