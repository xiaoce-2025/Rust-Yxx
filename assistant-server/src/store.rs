use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use serde::de::DeserializeOwned;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Sqlite, SqlitePool, Transaction};
use crate::services::{pkuvideo::PkuVideoJob, pkuvideo_fw::PkuVideoFwJob};
#[path = "store_accounts.rs"]
mod accounts;
#[path = "store_jobs.rs"]
mod jobs;
#[path = "store_migration.rs"]
mod migration;
#[path = "store_admin_list.rs"]
mod admin_list;
pub(crate) use admin_list::AdminList;
#[path = "store_sections.rs"]
mod sections;
#[path = "store_provider_config.rs"]
mod provider_config;
#[path = "store_legacy_features.rs"]
mod legacy_features;
use accounts::{replace_whitelist, write_session};

use uuid::Uuid;
#[path = "store_acceleration.rs"]
pub(crate) mod acceleration;
#[path = "store_operations.rs"]
mod operations;
#[path = "store_tex_cache.rs"]
mod tex_cache;
#[path = "store_note_billing.rs"]
pub mod note_billing;
#[path = "store_recharge.rs"]
mod recharge;
#[path = "store_video_storage.rs"]
mod video_storage;
pub use tex_cache::{tex_hash, reuse_cached_tex, GlobalTexEntry};
pub(crate) use tex_cache::{atomic_tex, valid_tex};
pub use operations::{OperationsQuery, SiteFeatures, FEATURES_PATH};




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
    pub processing_quota: i64,
    #[serde(default)]
    pub iaaa_encrypted_password: Option<String>,
    #[serde(default)]
    pub iaaa_public_key: Option<String>,
    #[serde(default)]
    pub iaaa_key_version: Option<String>,
    #[serde(default)]
    pub iaaa_username: Option<String>,
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

pub(crate) type StoreError = Box<dyn std::error::Error + Send + Sync>;

pub struct FileStore {
    tex_cache_lock: tokio::sync::Mutex<()>,
    data_dir: PathBuf,
    db: SqlitePool,
    provider_keys: provider_config::ProviderKeys,
}

async fn read_entity<'e, T: DeserializeOwned>(
    executor: impl sqlx::Executor<'e, Database = Sqlite>,
    table: &'static str,
    id: impl ToString,
) -> Result<Option<T>, StoreError> {
    let sql = format!("SELECT payload FROM {table} WHERE id=?");
    let payload: Option<String> = sqlx::query_scalar(&sql).bind(id.to_string()).fetch_optional(executor).await?;
    Ok(payload.as_deref().map(serde_json::from_str).transpose()?)
}

async fn write_entity(
    connection: &mut sqlx::SqliteConnection,
    table: &'static str,
    id: impl ToString,
    record: &impl Serialize,
) -> Result<(), StoreError> {
    let sql = format!("INSERT INTO {table}(id,payload) VALUES (?,?) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload");
    sqlx::query(&sql).bind(id.to_string()).bind(serde_json::to_string(record)?).execute(connection).await?;
    Ok(())
}

async fn write_document(
    connection: &mut sqlx::SqliteConnection,
    name: &str,
    payload: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO store_documents(name,payload) VALUES (?,?) ON CONFLICT(name) DO UPDATE SET payload=excluded.payload")
        .bind(name).bind(payload).execute(connection).await?;
    Ok(())
}

impl FileStore {
    pub async fn new(data_dir: PathBuf) -> Result<Self, StoreError> {
        tokio::fs::create_dir_all(&data_dir).await?;
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new()
                .filename(data_dir.join("assistant.sqlite3"))
                .create_if_missing(true)
                .foreign_keys(true)
                .busy_timeout(std::time::Duration::from_secs(5)))
            .await?;
        let provider_keys = provider_config::ProviderKeys::new(&data_dir);
        migration::migrate(&db, &data_dir, &provider_keys).await?;
        provider_keys.validate(&db).await?;
        let store = Self { data_dir, db, provider_keys, tex_cache_lock: tokio::sync::Mutex::new(()) };
        store.init_note_billing().await?;
        store.init_tex_cache().await?;
        store.init_operations().await?;
        Ok(store)
    }

    pub async fn verify_database(&self) -> Result<serde_json::Value, StoreError> {
        let integrity: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check").fetch_all(&self.db).await?;
        if integrity != ["ok"] {
            return Err("SQLite integrity_check 未通过".into());
        }
        if sqlx::query("PRAGMA foreign_key_check").fetch_optional(&self.db).await?.is_some() {
            return Err("SQLite foreign_key_check 未通过".into());
        }
        let mut counts = serde_json::Map::new();
        for table in [
            "users", "sessions", "oidc_identities", "registration_whitelist", "user_configs",
            "user_provider_configs", "conversations", "messages", "wechat_bindings",
            "wechat_channels", "oauth_states", "pkuvideo_jobs", "pkuvideo_fw_jobs",
            "translation_jobs", "course_lessons", "ta_drafts", "store_documents", "note_credit_accounts", "note_credit_ledger", "note_recharge_orders",
        ] {
            let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&self.db).await?;
            counts.insert(table.into(), count.into());
        }
        let (sessions_without_user, users_without_balance, links_without_user): (i64, i64, i64) =
            sqlx::query_as("SELECT
                (SELECT count(*) FROM sessions LEFT JOIN users ON users.id=sessions.user_id WHERE users.id IS NULL),
                (SELECT count(*) FROM users LEFT JOIN note_credit_accounts ON note_credit_accounts.user_id=users.id WHERE note_credit_accounts.user_id IS NULL),
                (SELECT count(*) FROM oidc_identities LEFT JOIN users ON users.id=oidc_identities.user_id WHERE users.id IS NULL)")
                .fetch_one(&self.db).await?;
        if users_without_balance > 0 || links_without_user > 0 {
            return Err(format!("迁移后有 {users_without_balance} 个账户缺少余额，{links_without_user} 条 OIDC 关联缺少用户").into());
        }
        let sqlite_version: String = sqlx::query_scalar("SELECT sqlite_version()").fetch_one(&self.db).await?;
        let schema_version: i64 = sqlx::query_scalar("SELECT max(version) FROM store_migrations").fetch_one(&self.db).await?;
        Ok(serde_json::json!({
            "sqlite_version": sqlite_version, "schema_version": schema_version, "row_counts": counts,
            "historical_sessions_without_user": sessions_without_user,
        }))
    }


    async fn read_entities<T: DeserializeOwned>(&self, sql: &str, parameter: Option<String>) -> Vec<T> {
        let mut query = sqlx::query_scalar::<_, String>(sql);
        if let Some(parameter) = parameter {
            query = query.bind(parameter);
        }
        match query.fetch_all(&self.db).await {
            Ok(records) => records.into_iter().filter_map(|payload| match serde_json::from_str(&payload) {
                Ok(record) => Some(record),
                Err(error) => { tracing::error!(%error, "解析数据库记录失败"); None }
            }).collect(),
            Err(error) => { tracing::error!(%error, "读取数据库记录失败"); Vec::new() }
        }
    }

    async fn delete_entity(&self, table: &'static str, id: impl ToString) -> Result<(), StoreError> {
        let sql = format!("DELETE FROM {table} WHERE id=?");
        sqlx::query(&sql).bind(id.to_string()).execute(&self.db).await?;
        Ok(())
    }

    fn sidecar_key(path: &std::path::Path) -> String {
        let parts: Vec<_> = path.components().map(|part| part.as_os_str().to_string_lossy()).collect();
        format!("sidecar/{}", parts.join("/"))
    }

    pub async fn stored_sidecar(&self, path: &std::path::Path) -> Result<Option<String>, String> {
        self.document(Self::sidecar_key(path)).await.map(Option::flatten)
    }

    async fn document(&self, key: String) -> Result<Option<Option<String>>, String> {
        sqlx::query_scalar("SELECT payload FROM store_documents WHERE name=?")
            .bind(key).fetch_optional(&self.db).await.map_err(|error| error.to_string())
    }

    pub async fn save_sidecar(&self, path: &std::path::Path, payload: &str) -> Result<(), String> {
        let mut connection = self.db.acquire().await.map_err(|error| error.to_string())?;
        write_document(&mut connection, &Self::sidecar_key(path), payload)
            .await.map_err(|error| error.to_string())
    }

    pub async fn sidecar_paths_under(&self, directory: &std::path::Path) -> Result<Vec<String>, String> {
        let base = Self::sidecar_key(directory);
        let prefix = format!("{base}/");
        let end = format!("{base}0");
        sqlx::query_scalar("SELECT substr(name,length(?1)+1) FROM store_documents WHERE name>=?2 AND name<?3 AND payload IS NOT NULL UNION SELECT substr(scope,length(?1)+1) FROM lesson_indexes WHERE scope>=?2 AND scope<?3 ORDER BY 1")
            .bind(&prefix).bind(&prefix).bind(end).fetch_all(&self.db).await.map_err(|error| error.to_string())
    }

    pub async fn delete_sidecar(&self, path: &std::path::Path) -> Result<(), String> {
        let mut connection = self.db.acquire().await.map_err(|error| error.to_string())?;
        Self::mask_document(&mut connection, path).await
    }

    async fn mask_document(connection: &mut sqlx::SqliteConnection, path: &std::path::Path) -> Result<(), String> {
        // A tombstone masks any old file that has yet to be removed from a backup.
        sqlx::query("INSERT INTO store_documents(name,payload) VALUES (?,NULL) ON CONFLICT(name) DO UPDATE SET payload=NULL")
            .bind(Self::sidecar_key(path)).execute(connection).await.map_err(|error| error.to_string())?;
        Ok(())
    }


    pub async fn load_sidecar<T: DeserializeOwned>(&self, path: &std::path::Path) -> Result<Option<T>, String> {
        let key = Self::sidecar_key(path);
        if let Some(payload) = self.document(key.clone()).await? {
            return payload.as_deref().map(serde_json::from_str).transpose().map_err(|error| error.to_string());
        }
        let payload = match tokio::fs::read_to_string(path).await {
            Ok(payload) => payload,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        serde_json::from_str::<T>(&payload).map_err(|error| format!("{}: {error}", path.display()))?;
        let payload: Option<String> = sqlx::query_scalar(
            "INSERT INTO store_documents(name,payload) VALUES (?,?)
             ON CONFLICT(name) DO UPDATE SET payload=store_documents.payload RETURNING payload",
        )
        .bind(key).bind(payload).fetch_one(&self.db).await.map_err(|error| error.to_string())?;
        payload.as_deref().map(serde_json::from_str).transpose().map_err(|error| error.to_string())
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

    #[tokio::test]
    async fn upgrade_preserves_sqlite_records_and_quarantines_legacy_files() {
        let dir = tempdir::TempDir::new_in("target", "sqlite-row-upgrade").unwrap();
        let user_id = Uuid::new_v4();
        let id = Uuid::new_v4();
        let now = Utc::now();
        let user = serde_json::json!({
            "id":user_id,"username":"student","email":"student@example.invalid","password_hash":"test",
            "created_at":now,"updated_at":now,"is_active":true,"processing_quota":7,"storage_quota":3
        });
        let session = serde_json::json!({
            "session_id":id,"user_id":user_id,"created_at":now,"expires_at":now+chrono::Duration::hours(1)
        });
        let config = serde_json::json!({
            "user_id":user_id,"default_model":"default","theme":"light","settings":{},"mimo_api_key":"test-provider-key"
        });
        let old = SqlitePoolOptions::new().max_connections(1).connect_with(
            SqliteConnectOptions::new().filename(dir.path().join("assistant.sqlite3")).create_if_missing(true),
        ).await.unwrap();
        sqlx::query("CREATE TABLE collections(name TEXT PRIMARY KEY,payload TEXT NOT NULL)")
            .execute(&old).await.unwrap();
        for (name, record_id, record) in [("users",user_id,user),("sessions",id,session),("configs",user_id,config)] {
            let payload = serde_json::to_string(&HashMap::from([(record_id,record)])).unwrap();
            sqlx::query("INSERT INTO collections VALUES (?,?)").bind(name).bind(payload).execute(&old).await.unwrap();
        }
        tokio::fs::write(dir.path().join("sessions.json"), "{invalid; SQLite takes precedence}").await.unwrap();
        tokio::fs::write(dir.path().join("whitelist.json"), r#"{"allowed_emails":["student@example.invalid"]}"#).await.unwrap();
        let legacy = dir.path().join("legacy.json");
        tokio::fs::write(&legacy, "true").await.unwrap();
        old.close().await;

        let store = FileStore::new(dir.path().into()).await.unwrap();
        assert_eq!(store.authenticated_user(id).await.unwrap().unwrap().id,user_id);
        assert_eq!(store.note_balance(user_id).await.unwrap(),7);
        assert!(store.is_email_in_whitelist("student@example.invalid").await);
        assert_eq!(store.get_config(user_id).await.unwrap().unwrap().mimo_api_key.as_deref(),Some("test-provider-key"));
        let ciphertext: Vec<u8> = sqlx::query_scalar("SELECT secret FROM user_provider_configs WHERE provider='mimo'")
            .fetch_one(&store.db).await.unwrap();
        assert!(!ciphertext.windows(b"test-provider-key".len()).any(|window| window == b"test-provider-key"));
        let metadata: String = sqlx::query_scalar("SELECT payload FROM user_configs").fetch_one(&store.db).await.unwrap();
        assert!(!metadata.contains("test-provider-key"));
        store.admin_revoke_sessions(user_id,None).await.unwrap();
        store.admin_save_whitelist(vec![]).await.unwrap();
        assert_eq!(store.load_sidecar::<bool>(&legacy).await.unwrap(),Some(true));
        store.delete_sidecar(&legacy).await.unwrap();
        assert_eq!(store.verify_database().await.unwrap()["row_counts"]["users"],1);
        drop(store);

        let reopened = FileStore::new(dir.path().into()).await.unwrap();
        assert!(reopened.authenticated_user(id).await.unwrap().is_none());
        assert!(!reopened.is_email_in_whitelist("student@example.invalid").await);
        assert_eq!(reopened.load_sidecar::<bool>(&legacy).await.unwrap(),None);
        assert!(legacy.exists());
        assert_eq!(reopened.note_balance(user_id).await.unwrap(),7);
        assert_eq!(reopened.get_config(user_id).await.unwrap().unwrap().mimo_api_key.as_deref(),Some("test-provider-key"));
    }

}
