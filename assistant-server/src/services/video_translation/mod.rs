//! An independent, resumable addition to downloaded FW videos. No legacy pipeline is invoked.
mod media;
mod providers;
mod subtitles;
mod timeline;

use crate::app_state::AppState;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{fs, io::AsyncWriteExt, sync::Mutex, task::JoinHandle};
use tracing::Instrument;
use uuid::Uuid;

pub const ROOT: &str = "./data/pkuvideo_fw_translations";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub id: usize,
    pub start_ms: u64,
    pub end_ms: u64,
    pub source: String,
    #[serde(default)]
    pub translation: String,
    #[serde(default)]
    pub spoken: String,
    #[serde(default)]
    pub revision: u8,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub audio_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslationJob {
    pub version: u32,
    pub id: Uuid,
    pub user_id: Uuid,
    pub status: String,
    pub stage: String,
    pub error: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub voice: String,
    pub tts_model: String,
    pub tts_base_url: String,
    pub deepseek_model: String,
    pub max_speed: f64,
    pub blocks: Vec<Block>,
    #[serde(default)]
    pub glossary: serde_json::Value,
    #[serde(default = "default_refinement")]
    pub refine_subtitles: bool,
    #[serde(default)]
    pub subtitle_precision: String,
}

fn default_refinement() -> bool {
    true
}

pub struct Entry {
    pub job: Mutex<TranslationJob>,
    task: Mutex<Option<JoinHandle<()>>>,
}

pub struct TranslationManager {
    pub entries: Mutex<HashMap<Uuid, Arc<Entry>>>,
    // Bound CPU, disk and provider use across all users; do not couple to download slots.
    worker: Mutex<()>,
}

impl TranslationManager {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            worker: Mutex::new(()),
        }
    }
    pub async fn entry(&self, id: Uuid) -> Option<Arc<Entry>> {
        self.entries.lock().await.get(&id).cloned()
    }
}

pub fn dir(id: Uuid) -> PathBuf {
    PathBuf::from(ROOT).join(id.to_string())
}

pub async fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
    file.write_all(&bytes).await.map_err(|e| e.to_string())?;
    file.sync_all().await.map_err(|e| e.to_string())?;
    drop(file);
    // Rename on Windows also replaces an existing file through Tokio's platform implementation.
    fs::rename(tmp, path).await.map_err(|e| e.to_string())
}

async fn save(entry: &Entry) -> Result<(), String> {
    let mut job = entry.job.lock().await;
    job.updated_at = Utc::now();
    atomic_json(&dir(job.id).join("manifest.json"), &*job).await
}

async fn stage(entry: &Entry, name: &str) -> Result<(), String> {
    {
        let mut job = entry.job.lock().await;
        job.stage = name.into();
        tracing::info!(target: "video_translation", job_id=%job.id, stage=name,
            completed_blocks=job.blocks.iter().filter(|b| b.done).count(), total_blocks=job.blocks.len(), "翻译任务阶段更新");
    }
    save(entry).await
}

pub async fn start(state: &AppState, id: Uuid, retry: bool) -> Result<TranslationJob, String> {
    let config = &state.settings.video_translation;
    if !config.enabled {
        return Err("视频翻译功能未开启".into());
    }
    if !config.max_speed.is_finite() || !(1.0..=1.3).contains(&config.max_speed) {
        return Err("翻译 max_speed 必须在 1.0–1.3 之间".into());
    }
    providers::Credentials::load(state, id).await?;
    let entry = match state.video_translations.entry(id).await {
        Some(entry) => {
            let job = entry.job.lock().await;
            if job.status == "deleting" {
                return Err("任务正在删除，请稍后重试".into());
            }
            if matches!(job.status.as_str(), "queued" | "running") {
                let snapshot = job.clone();
                drop(job);
                // Resume a queued entry whose previous manifest write/launch failed.
                save(&entry).await?;
                launch(state.clone(), entry.clone()).await;
                return Ok(snapshot);
            }
            if !retry || job.status == "succeeded" {
                return Ok(job.clone());
            }
            drop(job);
            // A terminal state can be observed just before the old worker's final save
            // returns. Join that worker before queuing a new one, without holding job locks.
            if let Some(previous) = entry.task.lock().await.take() {
                let _ = previous.await;
            }
            let mut job = entry.job.lock().await;
            job.status = "queued".into();
            job.error = None;
            drop(job);
            entry
        }
        None => {
            let credentials = providers::Credentials::load(state, id).await?;
            let job = TranslationJob {
                version: 1,
                id,
                user_id: credentials.user_id,
                status: "queued".into(),
                stage: "排队中".into(),
                error: None,
                updated_at: Utc::now(),
                duration_ms: 0,
                voice: config.voice.clone(),
                tts_model: credentials.mimo_model,
                tts_base_url: config.mimo_base_url.clone(),
                deepseek_model: credentials.model,
                max_speed: config.max_speed,
                blocks: Vec::new(),
                glossary: serde_json::json!({}),
                refine_subtitles: config.refine_subtitles,
                subtitle_precision: "block".into(),
            };
            fs::create_dir_all(dir(id))
                .await
                .map_err(|e| e.to_string())?;
            let entry = Arc::new(Entry {
                job: Mutex::new(job),
                task: Mutex::new(None),
            });
            state
                .video_translations
                .entries
                .lock()
                .await
                .insert(id, entry.clone());
            entry
        }
    };
    save(&entry).await?;
    tracing::info!(target: "video_translation", job_id=%id, retry, "翻译任务已入队");
    launch(state.clone(), entry.clone()).await;
    let job = entry.job.lock().await.clone();
    Ok(job)
}

async fn launch(state: AppState, entry: Arc<Entry>) {
    let mut handle = entry.task.lock().await;
    if handle.is_some() {
        return;
    }
    let work = entry.clone();
    let job = work.job.lock().await.clone();
    let span = tracing::info_span!(target: "video_translation", "translation_job", job_id=%job.id, user_id=%job.user_id);
    *handle = Some(tokio::spawn(async move {
        let queued = tokio::time::Instant::now();
        tracing::info!(target: "video_translation", "等待翻译执行槽位");
        let _permit = state.video_translations.worker.lock().await;
        let started = tokio::time::Instant::now();
        tracing::info!(target: "video_translation", queue_wait_ms=queued.elapsed().as_millis() as u64,
            tts_model=%job.tts_model, deepseek_model=%job.deepseek_model, voice=%job.voice, "开始执行翻译任务");
        work.job.lock().await.status = "running".into();
        let result = run(&state, &work).await;
        let mut job = work.job.lock().await;
        match result {
            Ok(()) => {
                job.status = "succeeded".into();
                job.stage = "翻译完成".into();
                job.error = None;
                tracing::info!(target: "video_translation", total_blocks=job.blocks.len(),
                    elapsed_ms=started.elapsed().as_millis() as u64, subtitle_precision=%job.subtitle_precision, "翻译任务完成，译制视频可播放与下载");
            }
            Err(error) => {
                job.status = if error.starts_with("对轴待处理") {
                    "needs_review"
                } else {
                    "failed"
                }
                .into();
                tracing::info!(target: "video_translation", status=%job.status, stage=%job.stage, %error,
                    elapsed_ms=started.elapsed().as_millis() as u64, "翻译任务停止，已保留断点");
                job.error = Some(error);
            }
        }
        drop(job);
        if let Err(error) = save(&work).await {
            tracing::error!(%error, "保存视频翻译状态失败");
        }
    }.instrument(span)));
}

pub async fn restore(state: &AppState) -> Result<(), String> {
    fs::create_dir_all(ROOT).await.map_err(|e| e.to_string())?;
    let mut dirs = fs::read_dir(ROOT).await.map_err(|e| e.to_string())?;
    while let Some(item) = dirs.next_entry().await.map_err(|e| e.to_string())? {
        let Ok(id) = Uuid::parse_str(&item.file_name().to_string_lossy()) else {
            continue;
        };
        let bytes = match fs::read(item.path().join("manifest.json")).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut job: TranslationJob = match serde_json::from_slice(&bytes) {
            Ok(job) => job,
            Err(error) => {
                tracing::error!(%id, %error, "翻译清单损坏，保留文件供恢复");
                continue;
            }
        };
        if job.id != id || job.version != 1 {
            continue;
        }
        if job.status == "succeeded"
            && !fs::try_exists(dir(id).join("translated.mp4"))
                .await
                .unwrap_or(false)
        {
            job.status = "failed".into();
            job.error = Some("译制文件缺失，请重试封装；已有识别和配音缓存会继续复用".into());
        }
        let source = state.pkuvideo_fw_jobs.read().await.get(&id).cloned();
        if source.as_ref().is_some_and(|s| s.user_id != job.user_id) {
            continue;
        }
        let deleting = job.status == "deleting" || source.is_none();
        let resume = matches!(job.status.as_str(), "running" | "queued");
        tracing::info!(target: "video_translation", job_id=%id, status=%job.status, resume, deleting,
            completed_blocks=job.blocks.iter().filter(|b| b.done).count(), total_blocks=job.blocks.len(), "已加载翻译任务断点");
        let entry = Arc::new(Entry {
            job: Mutex::new(job),
            task: Mutex::new(None),
        });
        state
            .video_translations
            .entries
            .lock()
            .await
            .insert(id, entry.clone());
        if deleting {
            if let Err(error) = crate::routes::pkuvideo_fw::remove_job(state, id).await {
                tracing::error!(%id, %error, "恢复视频任务删除失败，将在下次启动继续清理");
            }
        } else if resume && state.settings.video_translation.enabled {
            launch(state.clone(), entry).await;
        }
    }
    Ok(())
}

// Called under the original FW slot lock. Keep the tombstone until source cleanup succeeds.
pub async fn remove(state: &AppState, id: Uuid) -> Result<(), String> {
    if let Some(entry) = state.video_translations.entry(id).await {
        tracing::info!(target: "video_translation", job_id=%id, "正在取消翻译任务，准备清理文件");
        entry.job.lock().await.status = "deleting".into();
        // Stop the writer before saving a durable tombstone or touching its files.
        if let Some(task) = entry.task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
        save(&entry).await?;
    }
    Ok(())
}

pub async fn finish_remove(state: &AppState, id: Uuid) -> Result<(), String> {
    let target = dir(id);
    if fs::try_exists(&target).await.map_err(|e| e.to_string())? {
        let root = fs::canonicalize(ROOT).await.map_err(|e| e.to_string())?;
        let resolved = fs::canonicalize(&target).await.map_err(|e| e.to_string())?;
        if resolved == root || !resolved.starts_with(root) {
            return Err("翻译目录超出允许范围".into());
        }
        // A cancelled FFmpeg may need a moment to release handles on Windows.
        for attempt in 0..5 {
            match fs::remove_dir_all(&resolved).await {
                Ok(()) => break,
                Err(_) if attempt < 4 => {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await
                }
                Err(e) => return Err(format!("清理翻译文件失败，请重试删除：{e}")),
            }
        }
    }
    state.video_translations.entries.lock().await.remove(&id);
    tracing::info!(target: "video_translation", job_id=%id, "翻译任务文件与缓存清理完成");
    Ok(())
}

async fn run(state: &AppState, entry: &Entry) -> Result<(), String> {
    let snapshot = entry.job.lock().await.clone();
    let id = snapshot.id;
    let root = dir(id);
    let source = state
        .pkuvideo_fw_jobs
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or("原任务不存在")?;
    let source = PathBuf::from(source.video_path.ok_or("原视频未下载完成")?);
    let client = providers::Client::new(state, id).await?;
    let duration = media::duration(&source).await?;
    tracing::info!(target: "video_translation", duration_ms=duration, "原视频媒体时长已读取");
    entry.job.lock().await.duration_ms = duration;
    stage(entry, "提取音频").await?;
    let wav = root.join("source.wav");
    if media::wav_info(&wav).await.is_err() {
        media::extract(&source, &wav).await?;
        tracing::info!(target: "video_translation", "原视频音频提取完成");
    } else {
        tracing::info!(target: "video_translation", cache_hit=true, "复用已提取的原视频音频");
    }
    stage(entry, "英文语音识别").await?;
    let raw = client.asr(&wav, &root.join("source-asr"), "en").await?;
    if entry.job.lock().await.blocks.is_empty() {
        let blocks = timeline::blocks(&raw, duration)?;
        tracing::info!(target: "video_translation", total_blocks=blocks.len(), "英文识别结果已完成语义分块");
        entry.job.lock().await.blocks = blocks;
        save(entry).await?;
    }
    stage(entry, "翻译与术语处理").await?;
    if entry
        .job
        .lock()
        .await
        .glossary
        .as_object()
        .is_none_or(|g| g.is_empty())
    {
        let blocks = entry.job.lock().await.blocks.clone();
        tracing::info!(target: "video_translation", total_blocks=blocks.len(), "开始提取课程主题与术语表");
        let glossary = client.glossary(&blocks, &snapshot.deepseek_model).await?;
        tracing::info!(target: "video_translation", terms=glossary["terms"].as_array().map_or(0, |v| v.len()), "课程主题与术语表提取完成");
        entry.job.lock().await.glossary = glossary;
        save(entry).await?;
    } else {
        tracing::info!(target: "video_translation", cache_hit=true, "复用课程主题与术语表");
    }
    let count = entry.job.lock().await.blocks.len();
    for start in (0..count).step_by(8) {
        let job = entry.job.lock().await.clone();
        let end = (start + 8).min(count);
        if job.blocks[start..end]
            .iter()
            .all(|b| !b.translation.is_empty())
        {
            for index in start..end {
                tracing::info!(target: "video_translation", block=index+1, total_blocks=count, cache_hit=true, "翻译块完成：复用译文缓存");
            }
            continue;
        }
        stage(
            entry,
            &format!("翻译与术语处理 第{}–{end}/{count}块", start + 1),
        )
        .await?;
        for index in start..end {
            tracing::info!(target: "video_translation", block=index+1, total_blocks=count, batch_start=start+1, batch_end=end,
                "翻译块开始：与当前批次一起请求 DeepSeek");
        }
        let translated = client
            .translate(&job.blocks, start, end, &job.glossary, &job.deepseek_model)
            .await?;
        {
            let mut current = entry.job.lock().await;
            for (index, text) in translated {
                current.blocks[index].translation = text.clone();
                current.blocks[index].spoken = text;
                tracing::info!(target: "video_translation", block=index+1, total_blocks=count,
                    translated_chars=current.blocks[index].translation.chars().count(), "翻译块处理完成");
            }
        }
        save(entry).await?;
    }
    for index in 0..count {
        stage(entry, &format!("中文配音与对轴 {}/{count}", index + 1)).await?;
        let job = entry.job.lock().await.clone();
        let mut block = job.blocks[index].clone();
        let aligned = root.join(format!("aligned-{index}.wav"));
        if block.done && media::wav_info(&aligned).await.is_ok() {
            tracing::info!(target: "video_translation", block=index+1, total_blocks=count, cache_hit=true, audio_ms=block.audio_ms, "配音块完成：复用已对轴音频");
            continue;
        }
        block.done = false;
        let window = timeline::window(&job.blocks, index, duration)?;
        tracing::info!(target: "video_translation", block=index+1, total_blocks=count, start_ms=block.start_ms,
            window_ms=window, revision=block.revision, "开始处理配音块");
        loop {
            let tts = root.join(format!("tts-{index}-{}.wav", block.revision));
            if media::wav_info(&tts).await.is_err() {
                tracing::info!(target: "video_translation", block=index+1, total_blocks=count, revision=block.revision, "开始请求 MiMo 合成配音块");
                client.tts(&block.spoken, &tts, &job).await?;
            } else {
                tracing::info!(target: "video_translation", block=index+1, total_blocks=count, revision=block.revision, cache_hit=true, "复用 MiMo 配音缓存");
            }
            let info = media::wav_info(&tts).await?;
            tracing::info!(target: "video_translation", block=index+1, total_blocks=count, audio_ms=info.duration_ms(), window_ms=window, "配音块测长完成");
            if let Some(speed) = timeline::fit_speed(info.duration_ms(), window, job.max_speed) {
                media::align(&tts, &aligned, speed).await?;
                let measured = media::wav_info(&aligned).await?.duration_ms();
                tracing::info!(target: "video_translation", block=index+1, total_blocks=count, speed,
                    audio_ms=measured, window_ms=window, fits=measured<=window, "配音块变速与对轴测量完成");
                if measured <= window {
                    block.audio_ms = measured;
                    block.done = true;
                    tracing::info!(target: "video_translation", block=index+1, total_blocks=count, audio_ms=measured, revision=block.revision, "配音块处理完成");
                    break;
                }
            }
            if block.revision >= 2 {
                return Err(format!(
                    "对轴待处理：第 {} 块在两次压缩后仍超出时间窗，请检查译文",
                    index + 1
                ));
            }
            tracing::info!(target: "video_translation", block=index+1, total_blocks=count, attempt=block.revision+1,
                max_attempts=2, window_ms=window, "配音块超长，开始压缩译文并重新合成");
            block.spoken = client
                .compress(&block, window, &job.glossary, &job.deepseek_model)
                .await?;
            block.revision += 1;
            tracing::info!(target: "video_translation", block=index+1, total_blocks=count, revision=block.revision,
                spoken_chars=block.spoken.chars().count(), "配音块译文压缩完成");
            entry.job.lock().await.blocks[index] = block.clone();
            save(entry).await?;
        }
        entry.job.lock().await.blocks[index] = block;
        save(entry).await?;
    }
    stage(entry, "生成字幕与配音音轨").await?;
    let job = entry.job.lock().await.clone();
    let dub = root.join("dub.wav");
    media::assemble(&root, &job.blocks, duration, &dub).await?;
    let mut alignment_error = None;
    let refined = if job.refine_subtitles {
        stage(entry, "中文字幕时间轴校准").await?;
        match client.asr(&dub, &root.join("dub-asr"), "zh").await {
            Ok(raw) => Some(raw),
            Err(error) => {
                tracing::info!(target: "video_translation", %error, "配音复识别未成功，字幕将使用配音块边界");
                alignment_error = Some(error);
                None
            }
        }
    } else {
        None
    };
    let precision = subtitles::write(
        &root,
        &job.blocks,
        refined.as_ref(),
        alignment_error.as_deref(),
    )
    .await?;
    entry.job.lock().await.subtitle_precision = precision;
    save(entry).await?;
    stage(entry, "封装译制视频").await?;
    let output = root.join("translated.mp4");
    media::mux(&source, &dub, &root, &output, duration).await?;
    Ok(())
}

#[cfg(test)]
pub(crate) async fn test_state() -> (AppState, tempdir::TempDir) {
    let temp = tempdir::TempDir::new_in("target", "translation-state").unwrap();
    let settings = serde_json::from_value(serde_json::json!({
        "server":{"address":"127.0.0.1","port":0},
        "wechat":{"api_base_url":"http://127.0.0.1:1","oauth_app_id":"","oauth_app_secret":"","redirect_uri":"","qr_bot_type":""},
        "security":{"session_ttl_hours":1},"cors":{"allowed_origins":[],"allow_credentials":true},
        "audioapi_paraformer":{},"llm_deepseek":{}
    })).unwrap();
    let store = crate::store::FileStore::new(temp.path().into())
        .await
        .unwrap();
    (AppState::new(settings, Arc::new(store)), temp)
}

#[cfg(test)]
fn test_job(id: Uuid) -> TranslationJob {
    TranslationJob {
        version: 1,
        id,
        user_id: Uuid::new_v4(),
        status: "running".into(),
        stage: "test".into(),
        error: None,
        updated_at: Utc::now(),
        duration_ms: 1000,
        voice: "苏打".into(),
        tts_model: "mimo-v2.5-tts".into(),
        tts_base_url: "http://127.0.0.1:1".into(),
        deepseek_model: "test".into(),
        max_speed: 1.15,
        blocks: vec![],
        glossary: serde_json::json!({}),
        refine_subtitles: true,
        subtitle_precision: "block".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn cancelling_a_writer_preserves_tombstone_until_cleanup() {
        let (state, _temp) = test_state().await;
        let id = Uuid::new_v4();
        let other = Uuid::new_v4();
        fs::create_dir_all(dir(id)).await.unwrap();
        fs::create_dir_all(dir(other)).await.unwrap();
        fs::write(dir(other).join("keep"), b"unrelated")
            .await
            .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        struct DropSignal(Arc<AtomicBool>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let signal = dropped.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = DropSignal(signal);
            tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        rx.await.unwrap();
        let entry = Arc::new(Entry {
            job: Mutex::new(test_job(id)),
            task: Mutex::new(Some(task)),
        });
        state
            .video_translations
            .entries
            .lock()
            .await
            .insert(id, entry.clone());
        save(&entry).await.unwrap();
        remove(&state, id).await.unwrap();
        assert!(dropped.load(Ordering::SeqCst));
        let tombstone: TranslationJob =
            serde_json::from_slice(&fs::read(dir(id).join("manifest.json")).await.unwrap())
                .unwrap();
        assert_eq!(tombstone.status, "deleting");
        assert!(dir(id).exists());
        finish_remove(&state, id).await.unwrap();
        assert!(!dir(id).exists());
        assert!(state.video_translations.entry(id).await.is_none());
        assert_eq!(
            fs::read(dir(other).join("keep")).await.unwrap(),
            b"unrelated"
        );
        finish_remove(&state, other).await.unwrap();
    }

    #[tokio::test]
    async fn checkpoints_replace_atomically_and_keep_completed_blocks() {
        let temp = tempdir::TempDir::new_in("target", "translation-checkpoint").unwrap();
        let path = temp.path().join("manifest.json");
        let mut job = test_job(Uuid::new_v4());
        atomic_json(&path, &job).await.unwrap();
        job.blocks.push(Block {
            id: 0,
            start_ms: 0,
            end_ms: 1000,
            source: "source".into(),
            translation: "译文".into(),
            spoken: "配音".into(),
            revision: 2,
            done: true,
            audio_ms: 900,
        });
        atomic_json(&path, &job).await.unwrap();
        let recovered: TranslationJob =
            serde_json::from_slice(&fs::read(path).await.unwrap()).unwrap();
        assert!(recovered.blocks[0].done);
        assert_eq!(recovered.blocks[0].revision, 2);
        assert_eq!(recovered.blocks[0].spoken, "配音");
    }
}
