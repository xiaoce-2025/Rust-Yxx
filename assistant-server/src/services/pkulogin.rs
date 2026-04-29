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

struct PkuLoginResult {
    token: String,
    cookie_file: PathBuf,
    user_agent: String,
}

async fn login_iaaa_and_get_cookies(
    username: &str,
    password: &str,
) -> Result<PkuLoginResult, String> {
    let user_agent = random_user_agent("./user_agents.txt.gz")?;

    let cookie_file = std::env::temp_dir().join(format!(
        "pku_cookie_{}_{}.txt",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    ));
    let cookie_file_str = cookie_file.to_string_lossy().to_string();

    // Step 1: IAAA oauthlogin -> token
    let form_body = format!(
        "appid={}&userName={}&password={}&redirUrl={}",
        urlencoding::encode(BLACKBOARD_APP_ID),
        urlencoding::encode(username),
        urlencoding::encode(password),
        urlencoding::encode(BLACKBOARD_REDIR_URL)
    );

    let oauth_args = vec![
        "-sS".to_string(),
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

    let login_body = run_curl_text(&oauth_args).map_err(|e| format!("IAAA 登录请求失败: {e}"))?;
    let token = parse_token(&login_body)?;

    // Step 2: campusLogin?_rand=...&token=...
    let campus_urls = vec![
        format!(
            "{}?_rand={:.16}&token={}",
            BLACKBOARD_REDIR_URL,
            rand::random::<f64>(),
            urlencoding::encode(&token)
        ),
        format!(
            "{}?_rand={:.16}&token={}",
            BLACKBOARD_REDIR_URL_HTTPS,
            rand::random::<f64>(),
            urlencoding::encode(&token)
        ),
    ];

    let mut last_err = String::new();
    for campus_url in campus_urls {
        let campus_args = vec![
            "-sS".to_string(),
            "-L".to_string(),
            "-A".to_string(),
            user_agent.clone(),
            "-c".to_string(),
            cookie_file_str.clone(),
            "-b".to_string(),
            cookie_file_str.clone(),
            "-o".to_string(),
            "NUL".to_string(),
            campus_url,
        ];

        match run_curl_text(&campus_args) {
            Ok(_) => {
                last_err.clear();
                break;
            }
            Err(e) => {
                last_err = e;
            }
        }
    }

    if !last_err.is_empty() {
        let _ = fs::remove_file(&cookie_file);
        return Err(format!("教学网 token 登录失败: {last_err}"));
    }

    Ok(PkuLoginResult {
        token,
        cookie_file,
        user_agent,
    })
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
    let output = Command::new("curl.exe")
        .args(args)
        .output()
        .map_err(|e| format!("调用 curl.exe 失败: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("curl 返回非 0: {}", stderr.trim()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_curl_bytes(args: &[String]) -> Result<Vec<u8>, String> {
    let output = Command::new("curl.exe")
        .args(args)
        .output()
        .map_err(|e| format!("调用 curl.exe 失败: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("curl 返回非 0: {}", stderr.trim()));
    }

    Ok(output.stdout)
}

pub fn random_user_agent(gz_file: &str) -> Result<String, String> {
    let mut candidates = load_user_agents_from_plain_file("./user_agents.txt").unwrap_or_default();
    if candidates.is_empty() {
        candidates = load_user_agents_from_gzip_with_powershell(gz_file)?;
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

fn load_user_agents_from_gzip_with_powershell(path: &str) -> Result<Vec<String>, String> {
    let escaped_path = path.replace('\'', "''");
    let script = format!(
        "$p='{escaped_path}';$fs=[IO.File]::OpenRead($p);try{{$gz=New-Object IO.Compression.GzipStream($fs,[IO.Compression.CompressionMode]::Decompress);$sr=New-Object IO.StreamReader($gz,[Text.Encoding]::UTF8);try{{$sr.ReadToEnd()}}finally{{$sr.Dispose();$gz.Dispose()}}}}finally{{$fs.Dispose()}}"
    );

    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .map_err(|e| format!("调用 PowerShell 解压 user_agents 失败: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("PowerShell 解压 user_agents 失败: {}", stderr.trim()));
    }

    let content = String::from_utf8_lossy(&output.stdout);
    let candidates: Vec<String> = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect();

    Ok(candidates)
}
