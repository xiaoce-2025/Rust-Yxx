use std::{
    fs,
    fs::File,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::Command,
};

use rand::{seq::SliceRandom, thread_rng};
use serde_json::Value;

const IAAA_OAUTH_LOGIN_URL: &str = "https://iaaa.pku.edu.cn/iaaa/oauthlogin.do";
const BLACKBOARD_APP_ID: &str = "blackboard";
const BLACKBOARD_REDIR_URL: &str =
    "http://course.pku.edu.cn/webapps/bb-sso-BBLEARN/execute/authValidate/campusLogin";
const BLACKBOARD_REDIR_URL_HTTPS: &str =
    "https://course.pku.edu.cn/webapps/bb-sso-BBLEARN/execute/authValidate/campusLogin";

pub struct PkuLoginResult {
    pub token: String,
    pub cookie_file: PathBuf,
    pub user_agent: String,
}

pub async fn login_iaaa_and_get_cookies(
    username: &str,
    password: &str,
) -> Result<PkuLoginResult, String> {
    let start_time = chrono::Utc::now();
    tracing::info!("IAAA登录流程开始 - 用户名: {}, 时间: {}", username, start_time);

    let user_agent = random_user_agent("./user_agents.txt.gz")?;
    tracing::debug!("步骤1: 获取随机User-Agent成功 - {}", user_agent.chars().take(50).collect::<String>());

    let cookie_file = std::env::temp_dir().join(format!(
        "pku_cookie_{}_{}.txt",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    ));
    let cookie_file_str = cookie_file.to_string_lossy().to_string();
    tracing::debug!("步骤2: 创建临时Cookie文件 - {}", cookie_file_str);

    // Step 1: IAAA oauthlogin -> token
    let form_body = format!(
        "appid={}&userName={}&password={}&redirUrl={}",
        urlencoding::encode(BLACKBOARD_APP_ID),
        urlencoding::encode(username),
        urlencoding::encode(password),
        urlencoding::encode(BLACKBOARD_REDIR_URL)
    );
    tracing::debug!("步骤3: 准备OAuth登录请求 - body长度: {}", form_body.len());

    let oauth_args = vec![
        "-sS".to_string(),
        "-k".to_string(),
        "-A".to_string(),
        user_agent.clone(),
        "-H".to_string(),
        "Content-Type: application/x-www-form-urlencoded; charset=UTF-8".to_string(),
        "-c".to_string(),
        cookie_file_str.clone(),
        "-b".to_string(),
        cookie_file_str.clone(),
        "--data-raw".to_string(),
        form_body,
        IAAA_OAUTH_LOGIN_URL.to_string(),
    ];

    tracing::info!("步骤4: 开始执行IAAA OAuth登录请求...");
    let login_body = run_curl_text(&oauth_args).map_err(|e| {
        tracing::error!("IAAA OAuth登录请求失败: {}", e);
        format!("IAAA 登录请求失败: {e}")
    })?;
    tracing::info!("步骤4完成: IAAA OAuth登录请求返回 - body长度: {}", login_body.len());

    tracing::info!("步骤5: 解析IAAA响应Token...");
    let token = parse_token(&login_body)?;
    tracing::info!("步骤5完成: Token解析成功 - token长度: {}", token.len());

    // Step 2: campusLogin?_rand=...&token=...
    // 先尝试HTTPS，再尝试HTTP，避免HTTP超时等待
    let campus_urls = vec![
        format!(
            "{}?_rand={:.16}&token={}",
            BLACKBOARD_REDIR_URL_HTTPS,
            rand::random::<f64>(),
            urlencoding::encode(&token)
        ),
        format!(
            "{}?_rand={:.16}&token={}",
            BLACKBOARD_REDIR_URL,
            rand::random::<f64>(),
            urlencoding::encode(&token)
        ),
    ];
    tracing::debug!("步骤6: 准备教学网重定向URL - HTTPS: {}, HTTP: {}", campus_urls[0].len(), campus_urls[1].len());

    let mut last_err = String::new();
    for (idx, campus_url) in campus_urls.iter().enumerate() {
        tracing::info!("步骤7: 尝试第{}次教学网登录 (URL类型: {})...", idx + 1, if idx == 0 { "HTTPS" } else { "HTTP" });
        
        let campus_args = vec![
            "-sS".to_string(),
            "-k".to_string(),
            "-L".to_string(),
            "-A".to_string(),
            user_agent.clone(),
            "-c".to_string(),
            cookie_file_str.clone(),
            "-b".to_string(),
            cookie_file_str.clone(),
            "-o".to_string(),
            "/dev/null".to_string(),  // Linux/macOS
            campus_url.clone(),
        ];

        match run_curl_text(&campus_args) {
            Ok(_) => {
                tracing::info!("步骤7完成: 第{}次教学网登录成功", idx + 1);
                last_err.clear();
                break;
            }
            Err(e) => {
                tracing::warn!("步骤7: 第{}次教学网登录失败: {}", idx + 1, e);
                last_err = e;
            }
        }
    }

    if !last_err.is_empty() {
        let _ = fs::remove_file(&cookie_file);
        tracing::error!("教学网登录失败: {}", last_err);
        return Err(format!("教学网 token 登录失败: {last_err}"));
    }

    let end_time = chrono::Utc::now();
    let duration = end_time - start_time;
    tracing::info!("IAAA登录流程完成 - 总耗时: {}ms", duration.num_milliseconds());

    Ok(PkuLoginResult {
        token,
        cookie_file,
        user_agent,
    })
}

pub fn fetch_with_cookie(cookie_file: &str, user_agent: &str, url: &str) -> Result<String, String> {
    let args = vec![
        "-sS".to_string(),
        "-k".to_string(),
        "-L".to_string(),
        "-A".to_string(),
        user_agent.to_string(),
        "-c".to_string(),
        cookie_file.to_string(),
        "-b".to_string(),
        cookie_file.to_string(),
        url.to_string(),
    ];

    run_curl_text(&args)
}

pub async fn download_with_pku_cookie(
    url: &str,
    username: &str,
    password: &str,
) -> Result<Vec<u8>, String> {
    let login = login_iaaa_and_get_cookies(username, password).await?;

    // Keep the same cookie jar after campusLogin and use it to access the replay URL.
    let cookie_file_str = login.cookie_file.to_string_lossy().to_string();

    let args = vec![
        "-sS".to_string(),
        "-k".to_string(),
        "-L".to_string(),
        "-A".to_string(),
        login.user_agent,
        "-c".to_string(),
        cookie_file_str.clone(),
        "-b".to_string(),
        cookie_file_str,
        url.to_string(),
    ];

    let bytes = run_curl_bytes(&args).map_err(|e| format!("下载请求失败: {e}"));
    let _ = fs::remove_file(&login.cookie_file);
    bytes
}

fn parse_token(body: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| format!("解析 IAAA 登录响应 JSON 失败: {e}; body={body}"))?;

    if value.get("success").and_then(|v| v.as_bool()) == Some(false) {
        let msg = value
            .get("errors")
            .and_then(|v| v.get("msg"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        let code = value
            .get("errors")
            .and_then(|v| v.get("code"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown_code");
        return Err(format!("IAAA 登录失败: code={code}, msg={msg}"));
    }

    let token = value
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("IAAA 登录响应缺少 token 字段: {body}"))?;

    if token.trim().is_empty() {
        return Err("IAAA 登录 token 为空".to_string());
    }

    Ok(token.trim().to_string())
}

fn run_curl_text(args: &[String]) -> Result<String, String> {
    let curl_cmd = if cfg!(windows) { "curl.exe" } else { "curl" };
    let output = Command::new(curl_cmd)
        .args(args)
        .output()
        .map_err(|e| format!("调用 {} 失败: {}", curl_cmd, e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("curl 返回非 0: {}", stderr.trim()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_curl_bytes(args: &[String]) -> Result<Vec<u8>, String> {
    let curl_cmd = if cfg!(windows) { "curl.exe" } else { "curl" };
    let output = Command::new(curl_cmd)
        .args(args)
        .output()
        .map_err(|e| format!("调用 {} 失败: {}", curl_cmd, e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("curl 返回非 0: {}", stderr.trim()));
    }

    Ok(output.stdout)
}

pub fn random_user_agent(gz_file: &str) -> Result<String, String> {
    let mut candidates = load_user_agents_from_plain_file("./user_agents.txt").unwrap_or_default();
    if candidates.is_empty() {
        candidates = load_user_agents_from_gzip(gz_file)?;
    }

    if candidates.is_empty() {
        return Err("user_agents.txt.gz 中没有可用 user-agent".to_string());
    }

    candidates
        .choose(&mut thread_rng())
        .cloned()
        .ok_or_else(|| "随机选择 user-agent 失败".to_string())
}

fn load_user_agents_from_plain_file(path: &str) -> Result<Vec<String>, String> {
    let file = File::open(path).map_err(|e| format!("打开 {path} 失败: {e}"))?;
    let reader = BufReader::new(file);
    let mut candidates = Vec::new();

    for line in reader.lines() {
        let line = line.map_err(|e| format!("读取 {path} 失败: {e}"))?;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            candidates.push(trimmed.to_string());
        }
    }

    Ok(candidates)
}

fn load_user_agents_from_gzip(path: &str) -> Result<Vec<String>, String> {
    use std::io::Read;
    
    let file = File::open(path).map_err(|e| format!("打开 {path} 失败: {e}"))?;
    let mut gz = flate2::read::GzDecoder::new(file);
    let mut content = String::new();
    gz.read_to_string(&mut content).map_err(|e| format!("解压 {path} 失败: {e}"))?;

    let candidates: Vec<String> = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect();

    Ok(candidates)
}
