use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::{fs, process::Command};
use uuid::Uuid;

use crate::services::pkulogin::download_with_pku_cookie;
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
pub struct PipelineStep {
    pub name: String,
    pub status: PipelineStatus,
    pub message: String,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkuVideoJob {
    pub id: Uuid,
    pub user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub status: PipelineStatus,
    pub video_url: String,
    pub steps: Vec<PipelineStep>,
    pub artifacts_dir: String,
    pub transcript_path: Option<String>,
    pub notes_path: Option<String>,
    pub pdf_path: Option<String>,
    pub error: Option<String>,
}

impl PkuVideoJob {
    pub fn new(id: Uuid, user_id: Uuid, video_url: String, artifacts_dir: String) -> Self {
        let step_names = [
            "download_video",
            "extract_mp3",
            "speech_to_text",
            "extract_lecture_notes",
            "compile_pdf",
        ];

        Self {
            id,
            user_id,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            status: PipelineStatus::Pending,
            video_url,
            steps: step_names
                .iter()
                .map(|name| PipelineStep {
                    name: (*name).to_string(),
                    status: PipelineStatus::Pending,
                    message: "等待执行".to_string(),
                    started_at: None,
                    finished_at: None,
                })
                .collect(),
            artifacts_dir,
            transcript_path: None,
            notes_path: None,
            pdf_path: None,
            error: None,
        }
    }
}

pub async fn run_pipeline(
    job_id: Uuid,
    base_dir: PathBuf,
    video_url: String,
    login_credential_one: String,
    login_credential_two: String,
    settings: &crate::config::Settings,
    login_cache: &LoginCache,
) -> Result<(Option<String>, Option<String>, Option<String>), String> {
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let job_dir = base_dir.join(format!("{}_{}", timestamp, job_id));
    fs::create_dir_all(&job_dir)
        .await
        .map_err(|e| format!("创建任务目录失败: {e}"))?;

    let video_path = job_dir.join("video.mp4");
    let mp3_path = job_dir.join("audio.mp3");
    let transcript_path = job_dir.join("transcript.txt");
    let notes_path = job_dir.join("lecture_notes.tex");
    let pdf_path = job_dir.join("lecture_notes.pdf");

    download_video(
        &video_url,
        &video_path,
        &login_credential_one,
        &login_credential_two,
        login_cache,
    )
        .await
        .map_err(|e| format!("step=0;{e}"))?;
    extract_mp3(&video_path, &mp3_path)
        .await
        .map_err(|e| format!("step=1;{e}"))?;
    speech_to_text(&mp3_path, &transcript_path, settings)
        .await
        .map_err(|e| format!("step=2;{e}"))?;
    extract_notes(&transcript_path, &notes_path, settings)
        .await
        .map_err(|e| format!("step=3;{e}"))?;

    // PDF 编译失败不影响任务完成，只要 tex 文件生成成功即可
    let pdf_result = compile_pdf(&notes_path, &pdf_path).await;
    if let Err(ref e) = pdf_result {
        eprintln!("PDF 编译失败，但任务继续执行: {}", e);
    }

    // 将路径转换为相对路径（相对于 base_dir）
    let transcript_rel = transcript_path.strip_prefix(&base_dir)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| transcript_path.to_string_lossy().to_string());
    let notes_rel = notes_path.strip_prefix(&base_dir)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| notes_path.to_string_lossy().to_string());
    
    // PDF 路径只有在编译成功时才返回
    let pdf_rel = if pdf_result.is_ok() && pdf_path.exists() {
        pdf_path.strip_prefix(&base_dir)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| pdf_path.to_string_lossy().to_string())
    } else {
        "".to_string()
    };

    Ok((
        Some(transcript_rel),
        Some(notes_rel),
        if pdf_rel.is_empty() { None } else { Some(pdf_rel) },
    ))
}

pub async fn download_video(url: &str, video_path: &Path, username: &str, password: &str, login_cache: &LoginCache) -> Result<(), String> {
    let bytes = download_with_pku_cookie(url, username, password, login_cache).await?;

    fs::write(video_path, bytes)
        .await
        .map_err(|e| format!("写入视频文件失败: {e}"))
}

pub async fn extract_mp3(video_path: &Path, mp3_path: &Path) -> Result<(), String> {
    let ffmpeg_result = Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(video_path)
        .arg("-vn")
        .arg("-acodec")
        .arg("libmp3lame")
        .arg(mp3_path)
        .output()
        .await;

    match ffmpeg_result {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            eprintln!("ffmpeg执行失败: status={}, stdout={}, stderr={}", output.status, stdout, stderr);
            fs::write(mp3_path, b"[FFMPEG UNAVAILABLE] Placeholder mp3 content. Please install ffmpeg for real audio extraction.")
                .await
                .map_err(|e| format!("生成占位 mp3 失败: {e}"))
        }
        Err(e) => {
            eprintln!("ffmpeg命令执行错误: {}", e);
            fs::write(mp3_path, b"[FFMPEG UNAVAILABLE] Placeholder mp3 content. Please install ffmpeg for real audio extraction.")
                .await
                .map_err(|e| format!("生成占位 mp3 失败: {e}"))
        }
    }
}

pub async fn speech_to_text(mp3_path: &Path, transcript_path: &Path, settings: &crate::config::Settings) -> Result<(), String> {
    eprintln!("开始语音转文字处理");
    
    let api_key = std::env::var("DASHSCOPE_API_KEY")
        .ok()
        .or_else(|| settings.paraformer.api_key.clone())
        .ok_or("未设置 DASHSCOPE_API_KEY 环境变量且配置文件中未设置 api_key，请联系管理员配置阿里云语音识别API密钥")?;

    let base_url = settings.paraformer.base_url.as_deref()
        .unwrap_or("https://dashscope.aliyuncs.com/compatible-mode/v1");

    let model = settings.paraformer.model.as_deref()
        .ok_or("配置文件中未设置 model，请联系管理员配置阿里云语音识别模型")?;

    let upload_url = settings.paraformer.upload_url.as_deref()
        .unwrap_or("https://dashscope.aliyuncs.com/api/v1/uploads");

    let transcription_url = settings.paraformer.transcription_url.as_deref()
        .unwrap_or("https://dashscope.aliyuncs.com/api/v1/services/audio/asr/transcription");

    let task_query_url = settings.paraformer.task_query_url.as_deref()
        .unwrap_or("https://dashscope.aliyuncs.com/api/v1/tasks");

    let max_retries = settings.paraformer.max_retries.unwrap_or(120);

    eprintln!("使用API密钥: ******{}", &api_key[api_key.len().saturating_sub(6)..]);
    eprintln!("使用Base URL: {}", base_url);
    eprintln!("使用模型: {}", model);
    eprintln!("使用上传URL: {}", upload_url);
    eprintln!("使用转写URL: {}", transcription_url);
    eprintln!("使用任务查询URL: {}", task_query_url);
    eprintln!("使用最大重试次数: {}", max_retries);
    eprintln!("处理文件: {:?}", mp3_path);

    // 构建OSS URL存储文件路径
    let oss_url_file = transcript_path.with_file_name("oss_url.json");
    
    // 检查是否存在有效的OSS URL
    let oss_url = match check_existing_oss_url(&oss_url_file).await {
        Ok(url) => {
            eprintln!("使用缓存的OSS URL: {}", url);
            url
        },
        Err(_) => {
            // 重新上传文件
            let new_url = upload_file_to_oss(&api_key, mp3_path, model, upload_url).await?;
            eprintln!("文件上传成功，OSS URL: {}", new_url);
            
            // 保存OSS URL和上传时间
            save_oss_url(&oss_url_file, &new_url).await?;
            eprintln!("OSS URL已保存到: {:?}", oss_url_file);
            
            new_url
        }
    };

    let task_id = submit_transcription_task(&api_key, &oss_url, model, transcription_url).await?;
    eprintln!("任务提交成功，任务ID: {}", task_id);

    let transcription_result_url = wait_for_transcription_complete(&api_key, &task_id, task_query_url, max_retries).await?;
    eprintln!("转写完成，结果URL: {}", transcription_result_url);

    let transcript = download_transcription_result(&api_key, &transcription_result_url).await?;
    eprintln!("下载转写结果成功，文本长度: {}字符", transcript.len());

    fs::write(transcript_path, transcript)
        .await
        .map_err(|e| format!("写入转写文本失败: {e}"))
}

// 检查是否存在有效的OSS URL（44小时内）
async fn check_existing_oss_url(oss_url_file: &Path) -> Result<String, String> {
    if !oss_url_file.exists() {
        return Err("OSS URL文件不存在".to_string());
    }
    
    let content = fs::read_to_string(oss_url_file)
        .await
        .map_err(|e| format!("读取OSS URL文件失败: {}", e))?;
    
    #[derive(serde::Deserialize)]
    struct OSSUrlData {
        url: String,
        upload_time: String,
    }
    
    let data: OSSUrlData = serde_json::from_str(&content)
        .map_err(|e| format!("解析OSS URL文件失败: {}", e))?;
    
    // 解析上传时间
    let upload_time = chrono::DateTime::parse_from_rfc3339(&data.upload_time)
        .map_err(|e| format!("解析上传时间失败: {}", e))?;
    
    // 检查是否在44小时内
    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(upload_time);
    
    if duration.num_hours() > 44 {
        return Err("OSS URL已过期".to_string());
    }
    
    Ok(data.url)
}

// 保存OSS URL和上传时间
async fn save_oss_url(oss_url_file: &Path, url: &str) -> Result<(), String> {
    #[derive(serde::Serialize)]
    struct OSSUrlData {
        url: String,
        upload_time: String,
    }
    
    let data = OSSUrlData {
        url: url.to_string(),
        upload_time: chrono::Utc::now().to_rfc3339(),
    };
    
    let content = serde_json::to_string(&data)
        .map_err(|e| format!("序列化OSS URL数据失败: {}", e))?;
    
    fs::write(oss_url_file, content)
        .await
        .map_err(|e| format!("写入OSS URL文件失败: {}", e))
}

async fn upload_file_to_oss(api_key: &str, file_path: &Path, model: &str, upload_url: &str) -> Result<String, String> {
    eprintln!("开始上传文件到OSS");
    // 获取上传凭证的URL
    let get_policy_url = upload_url;

    let client = reqwest::Client::new();

    eprintln!("获取上传凭证: {}", get_policy_url);
    let policy_response = client
        .get(get_policy_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .query(&[("action", "getPolicy"), ("model", model)])
        .send()
        .await
        .map_err(|e| format!("获取上传凭证失败: {}", e))?;

    eprintln!("获取上传凭证响应状态: {}", policy_response.status());
    if !policy_response.status().is_success() {
        let error_text = policy_response.text().await.unwrap_or_default();
        eprintln!("获取上传凭证失败响应: {}", error_text);
        return Err(format!("获取上传凭证失败: {}", error_text));
    }

    let policy_data: serde_json::Value = policy_response.json()
        .await
        .map_err(|e| format!("解析上传凭证响应失败: {}", e))?;

    eprintln!("上传凭证响应: {:?}", policy_data);

    let data = policy_data.get("data")
        .and_then(|v: &serde_json::Value| v.as_object())
        .ok_or("响应中缺少data字段或data不是对象")?;

    let upload_host = data.get("upload_host")
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少upload_host字段")?;

    let upload_dir = data.get("upload_dir")
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少upload_dir字段")?;

    let oss_access_key_id = data.get("oss_access_key_id")
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少oss_access_key_id字段")?;

    let signature = data.get("signature")
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少signature字段")?;

    let policy = data.get("policy")
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少policy字段")?;

    let x_oss_object_acl = data.get("x_oss_object_acl")
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少x_oss_object_acl字段")?;

    let x_oss_forbid_overwrite = data.get("x_oss_forbid_overwrite")
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少x_oss_forbid_overwrite字段")?;

    let file_name = file_path.file_name()
        .and_then(|v| v.to_str())
        .ok_or("无法获取文件名")?;

    let key = format!("{}/{}", upload_dir, file_name);

    eprintln!("读取文件: {:?}", file_path);
    let file_bytes = fs::read(file_path)
        .await
        .map_err(|e| format!("读取文件失败: {}", e))?;

    eprintln!("文件大小: {}字节", file_bytes.len());
    eprintln!("上传到: {}", upload_host);
    eprintln!("OSS Key: {}", key);

    let multipart = reqwest::multipart::Form::new()
        .text("OSSAccessKeyId", oss_access_key_id.to_string())
        .text("Signature", signature.to_string())
        .text("policy", policy.to_string())
        .text("x-oss-object-acl", x_oss_object_acl.to_string())
        .text("x-oss-forbid-overwrite", x_oss_forbid_overwrite.to_string())
        .text("key", key.clone())
        .text("success_action_status", "200")
        .part("file", reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name.to_string())
            .mime_str("audio/mpeg")
            .map_err(|e| format!("设置MIME类型失败: {}", e))?);

    eprintln!("开始上传文件...");
    let upload_response = client
        .post(upload_host)
        .multipart(multipart)
        .send()
        .await
        .map_err(|e| format!("上传文件到OSS失败: {}", e))?;

    eprintln!("上传响应状态: {}", upload_response.status());
    if !upload_response.status().is_success() {
        let error_text = upload_response.text().await.unwrap_or_default();
        eprintln!("上传失败响应: {}", error_text);
        return Err(format!("上传文件到OSS失败: {}", error_text));
    }

    let oss_url = format!("oss://{}", key);
    eprintln!("上传成功，OSS URL: {}", oss_url);
    Ok(oss_url)
}

async fn submit_transcription_task(api_key: &str, file_url: &str, model: &str, transcription_url: &str) -> Result<String, String> {
    // 使用从配置文件传递过来的转写URL
    let submit_url = transcription_url;

    let client = reqwest::Client::new();

    let request_body = serde_json::json!({
        "model": model,
        "input": {
            "file_urls": [file_url]
        },
        "parameters": {
            "channel_id": [0]
        }
    });

    eprintln!("提交转写任务: {}", submit_url);
    eprintln!("请求体: {}", serde_json::to_string_pretty(&request_body).unwrap_or_default());
    
    let response = client
        .post(submit_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .header("X-DashScope-Async", "enable")
        .header("X-DashScope-OssResourceResolve", "enable")
        .json(&request_body)
        .send()
        .await
        .map_err(|e| format!("提交转写任务失败: {}", e))?;

    eprintln!("提交转写任务响应状态: {}", response.status());
    
    if !response.status().is_success() {
        let error_body = response.text().await.unwrap_or_default();
        eprintln!("提交转写任务失败响应: {}", error_body);
        return Err(format!("提交转写任务失败: {}", error_body));
    }

    let result: serde_json::Value = response.json()
        .await
        .map_err(|e| format!("解析转写任务响应失败: {}", e))?;

    let task_id = result.get("output")
        .and_then(|v| v.get("task_id"))
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少task_id字段")?;

    Ok(task_id.to_string())
}

async fn wait_for_transcription_complete(api_key: &str, task_id: &str, task_query_url: &str, max_retries: u32) -> Result<String, String> {
    // 使用从配置文件传递过来的任务查询URL
    let query_url = format!("{}/{}", task_query_url, task_id);

    let client = reqwest::Client::new();

    let retry_interval_secs = 5;

    for i in 0..max_retries {
        let response = client
            .get(&query_url)
            .header("Authorization", format!("Bearer {}", api_key))
            .send()
            .await
            .map_err(|e| format!("查询转写任务失败: {}", e))?;

        if !response.status().is_success() {
            return Err(format!("查询转写任务失败: {}", response.text().await.unwrap_or_default()));
        }

        let result: serde_json::Value = response.json()
            .await
            .map_err(|e| format!("解析查询响应失败: {}", e))?;

        let task_status = result.get("output")
            .and_then(|v| v.get("task_status"))
            .and_then(|v| v.as_str())
            .unwrap_or("PENDING");

        match task_status {
            "SUCCEEDED" => {
                let transcription_url = result.get("output")
                    .and_then(|v| v.get("results"))
                    .and_then(|v| v.as_array())
                    .and_then(|v| v.first())
                    .and_then(|v| v.get("transcription_url"))
                    .and_then(|v| v.as_str())
                    .ok_or("响应中缺少transcription_url字段")?;

                return Ok(transcription_url.to_string());
            }
            "FAILED" => {
                let error_msg = result.get("output")
                    .and_then(|v| v.get("results"))
                    .and_then(|v| v.as_array())
                    .and_then(|v| v.first())
                    .and_then(|v| v.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("未知错误");
                return Err(format!("转写任务失败: {}", error_msg));
            }
            _ => {
                eprintln!("等待转写完成... ({}/{})", i + 1, max_retries);
                tokio::time::sleep(tokio::time::Duration::from_secs(retry_interval_secs)).await;
            }
        }
    }

    Err(format!("转写任务超时（等待了{}秒）", max_retries as u64 * retry_interval_secs))
}

async fn download_transcription_result(api_key: &str, transcription_url: &str) -> Result<String, String> {
    let client = reqwest::Client::new();

    let response = client
        .get(transcription_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .await
        .map_err(|e| format!("下载转写结果失败: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("下载转写结果失败: {}", response.text().await.unwrap_or_default()));
    }

    let result: serde_json::Value = response.json()
        .await
        .map_err(|e| format!("解析转写结果JSON失败: {}", e))?;

    eprintln!("转写结果JSON响应: {}", serde_json::to_string_pretty(&result).unwrap_or_default());

    let sentences = result.get("transcripts")
        .and_then(|v| v.as_array())
        .ok_or("响应中缺少transcripts字段")?;

    let mut transcription_texts: Vec<String> = Vec::new();

    for sentence in sentences {
        let text = sentence.get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !text.is_empty() {
            transcription_texts.push(text.to_string());
        }
    }

    if transcription_texts.is_empty() {
        Ok("[未识别到语音内容]".to_string())
    } else {
        Ok(transcription_texts.join("\n"))
    }
}

pub async fn extract_notes(transcript_path: &Path, notes_path: &Path, settings: &crate::config::Settings) -> Result<(), String> {
    eprintln!("开始提取讲义");
    
    let transcript = fs::read_to_string(transcript_path)
        .await
        .map_err(|e| format!("读取转写文本失败: {e}"))?;

    // 读取系统提示词文件
    let system_prompt = fs::read_to_string("system_prompt.md")
        .await
        .map_err(|e| format!("读取系统提示词文件失败: {e}"))?;

    // 获取 DeepSeek API 配置
    let api_key = std::env::var("DEEPSEEK_API_KEY")
        .ok()
        .or_else(|| settings.deepseek.api_key.clone())
        .ok_or("未设置 DEEPSEEK_API_KEY 环境变量且配置文件中未设置 api_key，请联系管理员配置 DeepSeek API 密钥")?;

    let base_url = settings.deepseek.base_url.as_deref()
        .unwrap_or("https://api.deepseek.com");

    let model = settings.deepseek.model.as_deref()
        .unwrap_or("DeepSeek-V4-Flash");

    eprintln!("使用DeepSeek模型: {}", model);
    eprintln!("使用API基础URL: {}", base_url);
    eprintln!("转写文本长度: {}字符", transcript.len());

    // 构建请求
    let client = reqwest::Client::new();

    let request_body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": system_prompt
            },
            {
                "role": "user",
                "content": transcript
            }
        ],
        "temperature": 0.1,
        "extra_body": {
            "thinking": {
                "type": "enabled"
            }
        }
    });

    eprintln!("发送请求到DeepSeek API...");
    let response = client
        .post(format!("{}/chat/completions", base_url))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .await
        .map_err(|e| format!("调用DeepSeek API失败: {}", e))?;

    eprintln!("DeepSeek API响应状态: {}", response.status());

    if !response.status().is_success() {
        let error_body = response.text().await.unwrap_or_default();
        eprintln!("DeepSeek API失败响应: {}", error_body);
        return Err(format!("调用DeepSeek API失败: {}", error_body));
    }

    let result: serde_json::Value = response.json()
        .await
        .map_err(|e| format!("解析DeepSeek API响应失败: {}", e))?;

    // 提取回答内容（忽略 thinking/reasoning_content）
    let notes_content = result.get("choices")
        .and_then(|v| v.as_array())
        .and_then(|v| v.first())
        .and_then(|v| v.get("message"))
        .and_then(|v| v.get("content"))
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少content字段")?;

    eprintln!("讲义生成成功，原始内容长度: {}字符", notes_content.len());

    // 提取纯 LaTeX 内容（从 ```latex ... ``` 中提取）
    let pure_latex = extract_pure_latex(notes_content);
    eprintln!("提取纯 LaTeX 内容，长度: {}字符", pure_latex.len());

    // 将结果保存为 .tex 文件
    fs::write(notes_path, pure_latex)
        .await
        .map_err(|e| format!("写入讲义失败: {e}"))
}

/// 从内容中提取纯 LaTeX 代码
/// 规则：
/// 1. 只匹配 ```latex ... ``` 代码块，提取开始标记和最后一个 ``` 之间的内容
/// 2. 如果没有匹配到 latex 代码块，直接返回原文，不做任何修改
fn extract_pure_latex(content: &str) -> String {
    // 匹配 ```latex 开始标记的位置
    if let Some(start_pos) = content.find("```latex") {
        // 找到开始标记后，定位到标记结束位置（```latex 共9个字符）
        let after_start = &content[start_pos + 9..];
        
        // 查找最后一个 ``` 的位置
        if let Some(last_backtick_pos) = after_start.rfind("```") {
            // 返回开始标记后到最后一个 ``` 之前的内容
            let extracted = &after_start[..last_backtick_pos];
            return extracted.trim().to_string();
        }
    }
    
    // 如果没有匹配到 latex 代码块，直接返回原文
    content.to_string()
}

/// 将 LaTeX 章节文件编译为 PDF
pub async fn compile_pdf(notes_path: &Path, pdf_path: &Path) -> Result<(), String> {
    eprintln!("开始编译 PDF");
    
    // 获取任务目录
    let job_dir = notes_path.parent().ok_or("无法获取任务目录")?;
    
    // 转换为绝对路径以确保跨平台兼容性
    let job_dir_abs = job_dir.canonicalize().map_err(|e| format!("无法获取任务目录绝对路径: {e}"))?;
    eprintln!("任务目录(绝对路径): {:?}", job_dir_abs);
    
    // 检查笔记文件是否存在
    if !notes_path.exists() {
        return Err(format!("笔记文件不存在: {:?}", notes_path));
    }
    
    // 创建完整的 LaTeX 文档
    let full_tex = create_full_latex_doc(notes_path).await?;
    
    // 将完整文档写入临时文件
    let full_tex_path = job_dir_abs.join("full_document.tex");
    fs::write(&full_tex_path, full_tex)
        .await
        .map_err(|e| format!("写入完整文档失败: {e}"))?;
    
    eprintln!("完整文档路径: {:?}", full_tex_path);
    
    // 验证文件是否写入成功
    if !full_tex_path.exists() {
        return Err("完整文档写入失败，文件不存在".to_string());
    }
    
    // 复制模板文件到任务目录
    let template_cls = Path::new("./data/templates/elegantbook/elegantbook.cls");
    let template_cls_abs = template_cls.canonicalize().map_err(|e| format!("无法获取模板文件绝对路径: {e}"))?;
    
    if template_cls_abs.exists() {
        fs::copy(&template_cls_abs, job_dir_abs.join("elegantbook.cls"))
            .await
            .map_err(|e| format!("复制模板文件失败: {e}"))?;
        eprintln!("已复制模板文件");
    } else {
        return Err(format!("模板文件不存在: {:?}", template_cls_abs));
    }
    
    // 检查 xelatex 是否可用
    let xelatex_check = Command::new("xelatex").arg("--version").output().await;
    if xelatex_check.is_err() {
        return Err("xelatex 命令不可用，请确保已安装 TeX Live 或 MiKTeX".to_string());
    }
    
    // 使用 xelatex 编译
    eprintln!("开始执行 xelatex 编译...");
    
    // 将路径转换为使用正斜杠，确保跨平台兼容性（Windows 和 Linux）
    // 首先移除 Windows 扩展路径前缀 \\?\
    let full_tex_path_str = {
        let s = full_tex_path.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        s.replace('\\', "/")
    };
    
    let job_dir_str = {
        let s = job_dir_abs.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        s.replace('\\', "/")
    };
    
    eprintln!("编译路径: {}", full_tex_path_str);
    eprintln!("输出目录: {}", job_dir_str);
    
    let result = Command::new("xelatex")
        .arg("-interaction=nonstopmode")
        .arg("-output-directory")
        .arg(&job_dir_str)
        .arg(&full_tex_path_str)
        .current_dir(&job_dir_abs)
        .output()
        .await;
    
    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            
            if !output.status.success() {
                eprintln!("xelatex 编译警告: status={}", output.status);
                eprintln!("stdout: {}", stdout);
                eprintln!("stderr: {}", stderr);
            } else {
                eprintln!("xelatex 编译成功");
            }
            
            // 第二次编译以解决交叉引用
            let result2 = Command::new("xelatex")
                .arg("-interaction=nonstopmode")
                .arg("-output-directory")
                .arg(&job_dir_str)
                .arg(&full_tex_path_str)
                .current_dir(&job_dir_abs)
                .output()
                .await;
            
            if let Ok(output2) = result2 {
                if output2.status.success() {
                    eprintln!("第二次编译成功");
                } else {
                    eprintln!("第二次编译状态: {:?}", output2.status);
                }
            }
            
            // 检查 PDF 是否生成（这是判断是否成功的关键）
            let generated_pdf = job_dir_abs.join("full_document.pdf");
            eprintln!("检查 PDF 文件: {:?}", generated_pdf);
            if generated_pdf.exists() {
                // 复制到目标位置
                fs::copy(&generated_pdf, pdf_path)
                    .await
                    .map_err(|e| format!("复制 PDF 文件失败: {e}"))?;
                eprintln!("PDF 编译完成: {:?}", pdf_path);
                Ok(())
            } else {
                // 检查是否有 log 文件可以帮助诊断
                let log_path = job_dir_abs.join("full_document.log");
                eprintln!("检查日志文件: {:?}", log_path);
                if log_path.exists() {
                    if let Ok(log_content) = fs::read_to_string(&log_path).await {
                        // 获取最后几行日志
                        let last_lines: Vec<&str> = log_content.lines().rev().take(30).collect();
                        let last_lines_str: Vec<String> = last_lines.into_iter().rev().map(|s| s.to_string()).collect();
                        return Err(format!("PDF 文件未生成，编译日志最后30行:\n{}", last_lines_str.join("\n")));
                    }
                }
                Err(format!("PDF 文件未生成\nstdout: {}\nstderr: {}", stdout, stderr))
            }
        }
        Err(e) => {
            eprintln!("xelatex 命令执行错误: {}", e);
            Err(format!("xelatex 命令执行错误: {}", e))
        }
    }
}

/// 根据章节内容创建完整的 LaTeX 文档
async fn create_full_latex_doc(chapter_path: &Path) -> Result<String, String> {
    let chapter_content = fs::read_to_string(chapter_path)
        .await
        .map_err(|e| format!("读取章节内容失败: {e}"))?;
    
    let full_doc = format!("\\documentclass[lang=cn,10pt,scheme=chinese,a4paper]{{elegantbook}}\n\n\\title{{课堂讲义}}\n\\author{{自动生成}}\n\\date{{}}\n\n\\begin{{document}}\n\n\\mainmatter\n\n{}\n\n\\end{{document}}\n", chapter_content);
    
    Ok(full_doc)
}