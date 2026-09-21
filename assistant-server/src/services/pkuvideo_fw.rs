use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::fs;
use uuid::Uuid;

use crate::services::login_cache::LoginCache;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkuVideoFwJob {
    pub id: Uuid,
    pub user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub status: PipelineStatus,
    pub video_url: String,
    pub video_path: Option<String>,
    pub error: Option<String>,
    #[serde(skip)]
    pub progress: Option<Arc<AtomicU8>>,
}

impl PkuVideoFwJob {
    pub fn new(id: Uuid, user_id: Uuid, video_url: String) -> Self {
        Self {
            id,
            user_id,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            status: PipelineStatus::Pending,
            video_url,
            video_path: None,
            error: None,
            progress: None,
        }
    }

    pub fn get_progress(&self) -> Option<u8> {
        self.progress.as_ref().map(|p| p.load(Ordering::SeqCst))
    }
}

pub async fn download_video_only(
    job_id: Uuid,
    base_dir: PathBuf,
    video_url: String,
    login_credential_one: String,
    login_credential_two: String,
    login_cache: &LoginCache,
    manual_key: Option<&str>,
    concurrency: usize,
    segment_timeout_secs: u64,
    progress: Option<Arc<AtomicU8>>,
) -> Result<PathBuf, String> {
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let job_dir = base_dir.join(format!("{}_{}", timestamp, job_id));
    fs::create_dir_all(&job_dir)
        .await
        .map_err(|e| format!("创建任务目录失败: {e}"))?;

    let video_path = job_dir.join("video.mp4");

    super::pkuvideo::download_video(
        &video_url,
        &video_path,
        &login_credential_one,
        &login_credential_two,
        login_cache,
        manual_key,
        concurrency,
        segment_timeout_secs,
        progress,
    )
    .await
    .map_err(|e| format!("step=0;{e}"))?;

    Ok(video_path)
}

/// 下载并解析 m3u8，返回密钥 URI（用于前端弹窗展示）
pub async fn fetch_m3u8_key_uri(
    m3u8_url: &str,
    username: &str,
    password: &str,
    login_cache: &LoginCache,
) -> Result<String, String> {
    super::pkuvideo::fetch_m3u8_key_uri(m3u8_url, username, password, login_cache).await
}