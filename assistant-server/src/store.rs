use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;



#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileUser {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub password_hash: String,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub storage_quota: i32,
    pub processing_quota: i32,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileUserConfig {
    pub user_id: Uuid,
    pub openai_api_key: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub default_model: String,
    pub theme: String,
    pub settings: serde_json::Value,
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

pub struct FileStore {
    data_dir: PathBuf,
    users: Arc<RwLock<HashMap<Uuid, FileUser>>>,
    sessions: Arc<RwLock<HashMap<Uuid, FileUserSession>>>,
    configs: Arc<RwLock<HashMap<Uuid, FileUserConfig>>>,
    conversations: Arc<RwLock<HashMap<Uuid, FileConversation>>>,
    messages: Arc<RwLock<Vec<FileConversationMessage>>>,
    wechat_bindings: Arc<RwLock<Vec<FileUserWeChatBinding>>>,
    wechat_channels: Arc<RwLock<HashMap<Uuid, FileUserWeChatChannel>>>,
    oauth_states: Arc<RwLock<Vec<FileWeChatOAuthState>>>,
    pkuvideo_jobs: Arc<RwLock<HashMap<Uuid, crate::services::pkuvideo::PkuVideoJob>>>,
}

impl FileStore {
    pub async fn new(data_dir: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        tokio::fs::create_dir_all(&data_dir).await?;

        let store = Self {
            data_dir,
            users: Arc::new(RwLock::new(HashMap::new())),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            configs: Arc::new(RwLock::new(HashMap::new())),
            conversations: Arc::new(RwLock::new(HashMap::new())),
            messages: Arc::new(RwLock::new(Vec::new())),
            wechat_bindings: Arc::new(RwLock::new(Vec::new())),
            wechat_channels: Arc::new(RwLock::new(HashMap::new())),
            oauth_states: Arc::new(RwLock::new(Vec::new())),
            pkuvideo_jobs: Arc::new(RwLock::new(HashMap::new())),
        };

        store.load_all().await?;
        Ok(store)
    }

    async fn load_all(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.load_users().await.ok();
        self.load_sessions().await.ok();
        self.load_configs().await.ok();
        self.load_conversations().await.ok();
        self.load_messages().await.ok();
        self.load_wechat_bindings().await.ok();
        self.load_wechat_channels().await.ok();
        self.load_oauth_states().await.ok();
        self.load_pkuvideo_jobs().await.ok();
        Ok(())
    }

    // ===== Users =====
    async fn load_users(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("users.json");
        if path.exists() {
            let data = tokio::fs::read_to_string(&path).await?;
            let users: HashMap<Uuid, FileUser> = serde_json::from_str(&data)?;
            *self.users.write().await = users;
        }
        Ok(())
    }

    async fn save_users(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("users.json");
        let data = self.users.read().await;
        let json = serde_json::to_string_pretty(&*data)?;
        tokio::fs::write(&path, json).await?;
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

    pub async fn insert_user(&self, user: FileUser) -> Result<(), Box<dyn std::error::Error>> {
        self.users.write().await.insert(user.id, user);
        self.save_users().await?;
        Ok(())
    }

    pub async fn decrease_processing_quota(&self, user_id: Uuid) -> Result<(), Box<dyn std::error::Error>> {
        let mut users = self.users.write().await;
        if let Some(user) = users.get_mut(&user_id) {
            if user.processing_quota > 0 {
                user.processing_quota -= 1;
                user.updated_at = chrono::Utc::now();
                self.save_users().await?;
            }
        }
        Ok(())
    }

    pub async fn increase_storage_quota(&self, user_id: Uuid, amount: i32) -> Result<(), Box<dyn std::error::Error>> {
        let mut users = self.users.write().await;
        if let Some(user) = users.get_mut(&user_id) {
            user.storage_quota += amount;
            user.updated_at = chrono::Utc::now();
            self.save_users().await?;
        }
        Ok(())
    }

    // ===== Sessions =====
    async fn load_sessions(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("sessions.json");
        if path.exists() {
            let data = tokio::fs::read_to_string(&path).await?;
            let sessions: HashMap<Uuid, FileUserSession> = serde_json::from_str(&data)?;
            *self.sessions.write().await = sessions;
        }
        Ok(())
    }

    async fn save_sessions(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("sessions.json");
        let data = self.sessions.read().await;
        let json = serde_json::to_string_pretty(&*data)?;
        tokio::fs::write(&path, json).await?;
        Ok(())
    }

    pub async fn get_session(&self, session_id: Uuid) -> Option<FileUserSession> {
        let sessions = self.sessions.read().await;
        sessions.get(&session_id).cloned()
    }

    pub async fn insert_session(&self, session: FileUserSession) -> Result<(), Box<dyn std::error::Error>> {
        self.sessions.write().await.insert(session.session_id, session);
        self.save_sessions().await?;
        Ok(())
    }

    pub async fn delete_session(&self, session_id: Uuid) -> Result<(), Box<dyn std::error::Error>> {
        self.sessions.write().await.remove(&session_id);
        self.save_sessions().await?;
        Ok(())
    }

    // ===== Configs =====
    async fn load_configs(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("configs.json");
        if path.exists() {
            let data = tokio::fs::read_to_string(&path).await?;
            let configs: HashMap<Uuid, FileUserConfig> = serde_json::from_str(&data)?;
            *self.configs.write().await = configs;
        }
        Ok(())
    }

    async fn save_configs(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("configs.json");
        let data = self.configs.read().await;
        let json = serde_json::to_string_pretty(&*data)?;
        tokio::fs::write(&path, json).await?;
        Ok(())
    }

    pub async fn get_config(&self, user_id: Uuid) -> Option<FileUserConfig> {
        self.configs.read().await.get(&user_id).cloned()
    }

    pub async fn upsert_config(&self, config: FileUserConfig) -> Result<(), Box<dyn std::error::Error>> {
        self.configs.write().await.insert(config.user_id, config);
        self.save_configs().await?;
        Ok(())
    }

    // ===== Conversations =====
    async fn load_conversations(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("conversations.json");
        if path.exists() {
            let data = tokio::fs::read_to_string(&path).await?;
            let convs: HashMap<Uuid, FileConversation> = serde_json::from_str(&data)?;
            *self.conversations.write().await = convs;
        }
        Ok(())
    }

    async fn save_conversations(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("conversations.json");
        let data = self.conversations.read().await;
        let json = serde_json::to_string_pretty(&*data)?;
        tokio::fs::write(&path, json).await?;
        Ok(())
    }

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

    pub async fn insert_conversation(&self, conv: FileConversation) -> Result<(), Box<dyn std::error::Error>> {
        self.conversations.write().await.insert(conv.id, conv);
        self.save_conversations().await?;
        Ok(())
    }

    pub async fn update_conversation(&self, conv: FileConversation) -> Result<(), Box<dyn std::error::Error>> {
        self.conversations.write().await.insert(conv.id, conv);
        self.save_conversations().await?;
        Ok(())
    }

    // ===== Messages =====
    async fn load_messages(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("messages.json");
        if path.exists() {
            let data = tokio::fs::read_to_string(&path).await?;
            let msgs: Vec<FileConversationMessage> = serde_json::from_str(&data)?;
            *self.messages.write().await = msgs;
        }
        Ok(())
    }

    async fn save_messages(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("messages.json");
        let data = self.messages.read().await;
        let json = serde_json::to_string_pretty(&*data)?;
        tokio::fs::write(&path, json).await?;
        Ok(())
    }

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

    pub async fn insert_message(&self, msg: FileConversationMessage) -> Result<(), Box<dyn std::error::Error>> {
        self.messages.write().await.push(msg);
        self.save_messages().await?;
        Ok(())
    }

    // ===== WeChat Bindings =====
    async fn load_wechat_bindings(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("wechat_bindings.json");
        if path.exists() {
            let data = tokio::fs::read_to_string(&path).await?;
            let bindings: Vec<FileUserWeChatBinding> = serde_json::from_str(&data)?;
            *self.wechat_bindings.write().await = bindings;
        }
        Ok(())
    }

    async fn save_wechat_bindings(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("wechat_bindings.json");
        let data = self.wechat_bindings.read().await;
        let json = serde_json::to_string_pretty(&*data)?;
        tokio::fs::write(&path, json).await?;
        Ok(())
    }

    pub async fn get_wechat_binding(&self, user_id: Uuid) -> Option<FileUserWeChatBinding> {
        let bindings = self.wechat_bindings.read().await;
        bindings
            .iter()
            .filter(|b| b.user_id == user_id)
            .max_by_key(|b| b.updated_at)
            .cloned()
    }

    pub async fn upsert_wechat_binding(&self, binding: FileUserWeChatBinding) -> Result<(), Box<dyn std::error::Error>> {
        let mut bindings = self.wechat_bindings.write().await;
        if let Some(pos) = bindings.iter_mut().position(|b| b.user_id == binding.user_id && b.openid == binding.openid) {
            bindings[pos] = binding;
        } else {
            bindings.push(binding);
        }
        drop(bindings);
        self.save_wechat_bindings().await?;
        Ok(())
    }

    pub async fn delete_wechat_binding(&self, user_id: Uuid) -> Result<(), Box<dyn std::error::Error>> {
        let mut bindings = self.wechat_bindings.write().await;
        bindings.retain(|b| b.user_id != user_id);
        drop(bindings);
        self.save_wechat_bindings().await?;
        Ok(())
    }

    // ===== WeChat Channels =====
    async fn load_wechat_channels(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("wechat_channels.json");
        if path.exists() {
            let data = tokio::fs::read_to_string(&path).await?;
            let channels: Vec<FileUserWeChatChannel> = serde_json::from_str(&data)?;
            let mut map = HashMap::new();
            for ch in channels {
                map.insert(ch.user_id, ch);
            }
            *self.wechat_channels.write().await = map;
        }
        Ok(())
    }

    async fn save_wechat_channels(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("wechat_channels.json");
        let channels: Vec<_> = self.wechat_channels.read().await.values().cloned().collect();
        let json = serde_json::to_string_pretty(&channels)?;
        tokio::fs::write(&path, json).await?;
        Ok(())
    }

    pub async fn get_wechat_channel(&self, user_id: Uuid) -> Option<FileUserWeChatChannel> {
        self.wechat_channels.read().await.get(&user_id).cloned()
    }

    pub async fn upsert_wechat_channel(&self, channel: FileUserWeChatChannel) -> Result<(), Box<dyn std::error::Error>> {
        self.wechat_channels.write().await.insert(channel.user_id, channel);
        self.save_wechat_channels().await?;
        Ok(())
    }

    pub async fn get_all_wechat_channels(&self) -> Vec<FileUserWeChatChannel> {
        self.wechat_channels.read().await.values().cloned().collect()
    }

    // ===== OAuth States =====
    async fn load_oauth_states(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("oauth_states.json");
        if path.exists() {
            let data = tokio::fs::read_to_string(&path).await?;
            let states: Vec<FileWeChatOAuthState> = serde_json::from_str(&data)?;
            *self.oauth_states.write().await = states;
        }
        Ok(())
    }

    async fn save_oauth_states(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("oauth_states.json");
        let data = self.oauth_states.read().await;
        let json = serde_json::to_string_pretty(&*data)?;
        tokio::fs::write(&path, json).await?;
        Ok(())
    }

    pub async fn get_oauth_state(&self, state: &str) -> Option<(Uuid, DateTime<Utc>)> {
        self.oauth_states
            .read()
            .await
            .iter()
            .find(|s| s.state == state && s.expires_at > Utc::now())
            .map(|s| (s.user_id, s.expires_at))
    }

    pub async fn insert_oauth_state(&self, state: FileWeChatOAuthState) -> Result<(), Box<dyn std::error::Error>> {
        self.oauth_states.write().await.push(state);
        self.save_oauth_states().await?;
        Ok(())
    }

    pub async fn delete_oauth_state(&self, state: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut states = self.oauth_states.write().await;
        states.retain(|s| s.state != state);
        drop(states);
        self.save_oauth_states().await?;
        Ok(())
    }

    // ===== PKU Video Jobs =====
    async fn load_pkuvideo_jobs(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("pkuvideo_jobs.json");
        if path.exists() {
            let data = tokio::fs::read_to_string(&path).await?;
            let jobs: HashMap<Uuid, crate::services::pkuvideo::PkuVideoJob> = serde_json::from_str(&data)?;
            *self.pkuvideo_jobs.write().await = jobs;
        }
        Ok(())
    }

    async fn save_pkuvideo_jobs(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.data_dir.join("pkuvideo_jobs.json");
        let data = self.pkuvideo_jobs.read().await;
        let json = serde_json::to_string_pretty(&*data)?;
        tokio::fs::write(&path, json).await?;
        Ok(())
    }

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

    pub async fn insert_pkuvideo_job(&self, job: crate::services::pkuvideo::PkuVideoJob) -> Result<(), Box<dyn std::error::Error>> {
        self.pkuvideo_jobs.write().await.insert(job.id, job);
        self.save_pkuvideo_jobs().await?;
        Ok(())
    }

    pub async fn update_pkuvideo_job(&self, job: crate::services::pkuvideo::PkuVideoJob) -> Result<(), Box<dyn std::error::Error>> {
        self.pkuvideo_jobs.write().await.insert(job.id, job);
        self.save_pkuvideo_jobs().await?;
        Ok(())
    }

    pub async fn delete_pkuvideo_job(&self, job_id: Uuid) -> Result<(), Box<dyn std::error::Error>> {
        self.pkuvideo_jobs.write().await.remove(&job_id);
        self.save_pkuvideo_jobs().await?;
        Ok(())
    }

    pub async fn get_all_pkuvideo_jobs(&self) -> HashMap<Uuid, crate::services::pkuvideo::PkuVideoJob> {
        self.pkuvideo_jobs.read().await.clone()
    }
}
