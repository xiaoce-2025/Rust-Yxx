use std::{
    fs,
    fs::File,
    io::{BufRead, BufReader},
    process::Command,
};

use chrono::Utc;
use rand::{seq::SliceRandom, thread_rng};
use serde_json::Value;

use crate::services::login_cache::LoginCache;

const IAAA_OAUTH_LOGIN_URL: &str = "https://iaaa.pku.edu.cn/iaaa/oauthlogin.do";
const BLACKBOARD_APP_ID: &str = "blackboard";
const BLACKBOARD_REDIR_URL: &str =
    "http://course.pku.edu.cn/webapps/bb-sso-BBLEARN/execute/authValidate/campusLogin";
const BLACKBOARD_REDIR_URL_HTTPS: &str =
    "https://course.pku.edu.cn/webapps/bb-sso-BBLEARN/execute/authValidate/campusLogin";

pub type PkuLoginResult = super::login_cache::LoginCacheEntry;

pub const AUTH_EXPIRED: &str = "教学网登录已失效";

pub fn validate_page(content: &str) -> Result<(), String> {
    let lower = content.to_ascii_lowercase();
    let document = scraper::Html::parse_document(content);
    let password = scraper::Selector::parse("input[type='password' i]").unwrap();
    if document.select(&password).next().is_some()
        || lower.contains("form-pku-login") || lower.contains("/iaaa/oauth.jsp")
        || lower.contains("/webapps/login/?action=relogin")
        || content.contains("登录已失效") || content.contains("会话已过期") {
        return Err(AUTH_EXPIRED.into());
    }
    if content.contains("无权访问") || content.contains("没有权限访问") {
        return Err("教学网页面拒绝访问".into());
    }
    Ok(())
}

pub fn is_auth_error(error: &str) -> bool {
    error.contains(AUTH_EXPIRED) || error.contains("401")
}

/// Retry only explicit authentication failures, at most once per operation.
/// The session is otherwise pinned: this does not inspect its age.
pub async fn session_operation<T>(
    login: &mut PkuLoginResult, username: &str, password: &str, cache: &LoginCache,
    operation: impl FnMut(&PkuLoginResult) -> Result<T, String>,
) -> Result<T, String> {
    session_operation_with_refresh(login, operation,
        || login_iaaa_and_get_cookies_with_cache(username, password, cache)).await
}

async fn session_operation_with_refresh<T, F, Fut>(
    login: &mut PkuLoginResult,
    mut operation: impl FnMut(&PkuLoginResult) -> Result<T, String>,
    refresh: F,
) -> Result<T, String>
where F: FnOnce() -> Fut, Fut: std::future::Future<Output = Result<PkuLoginResult, String>> {
    let result = operation(login);
    if let Err(ref error) = result {
        login.invalidate();
        if is_auth_error(error) {
            *login = refresh().await?;
            return login.checked(operation(login));
        }
    }
    result
}

pub fn fetch_page_in_session(login: &PkuLoginResult, url: &str) -> Result<String, String> {
    let _guard = login.lock_cookie();
    let result = fetch_with_cookie(&login.cookie_file.to_string_lossy(), &login.user_agent, url)
        .and_then(|page| { validate_page(&page)?; Ok(page) });
    login.checked(result)
}

fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// Use the browser-style cookie jar, including domain/path scoped and HttpOnly cookies.
/// Do not replace it with a manually constructed Cookie header.
pub fn download_in_session(login: &PkuLoginResult, url: &str) -> Result<Vec<u8>, String> {
    download_in_session_with_timeout(login, url, Some(60))
}

/// Whole videos may legitimately take much longer than page/key requests.
pub fn download_video_in_session(login: &PkuLoginResult, url: &str) -> Result<Vec<u8>, String> {
    download_in_session_with_timeout(login, url, None)
}

fn download_in_session_with_timeout(login: &PkuLoginResult, url: &str, max_seconds: Option<u64>) -> Result<Vec<u8>, String> {
    let _guard = login.lock_cookie();
    let cookie_file = login.cookie_file.to_string_lossy().to_string();
    let mut args = vec![
        "-sS".into(), "-k".into(), "-L".into(),
        "--connect-timeout".into(), "15".into(),
        "-A".into(), login.user_agent.clone(),
        "-b".into(), cookie_file.clone(),
        "-c".into(), cookie_file,
    ];
    if let Some(seconds) = max_seconds {
        args.extend(["--max-time".into(), seconds.to_string()]);
    }
    args.push(url.into());
    login.checked(run_curl_bytes(&args).and_then(|bytes| {
        if let Ok(text) = std::str::from_utf8(&bytes) { validate_page(text)?; }
        Ok(bytes)
    }))
}

pub async fn login_iaaa_and_get_cookies_with_cache(
    username: &str,
    password: &str,
    login_cache: &LoginCache,
) -> Result<PkuLoginResult, String> {
    let lock = login_cache.login_lock(username).await;
    let _guard = lock.lock().await;
    if let Some(entry) = login_cache.get(username).await { return Ok(entry); }
    let result = login_iaaa_and_get_cookies(username, password).await?;
    Ok(login_cache.insert(username, result).await)
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
        uuid::Uuid::new_v4()
    ));
    let mut session = PkuLoginResult::new(cookie_file.clone(), user_agent.clone(), String::new());
    let cookie_file_str = cookie_file.to_string_lossy().to_string();
    tracing::debug!("步骤2: 创建临时Cookie文件 - {}", cookie_file_str);

    // Step 1: IAAA oauthlogin -> token
    let form_body = format!(
        "appid={}&userName={}&password={}&remTrustChk=false&redirUrl={}",
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
            "--fail".to_string(),
            campus_url.clone(),
        ];

        match run_curl_text(&campus_args) {
            Ok(page) if validate_page(&page).is_ok() => {
                // SSO callbacks may return only a redirect script; verify the portal itself.
                let portal = fetch_page_in_session(&session,
                    "https://course.pku.edu.cn/webapps/portal/execute/tabs/tabAction?tab_tab_group_id=_1_1");
                match portal {
                    Ok(portal) if is_teaching_portal(&portal) => {},
                    Ok(_) => { last_err = "教学网登录后未返回可识别的门户页面".into(); continue; },
                    Err(error) => { last_err = error; continue; },
                }
                tracing::info!("步骤7完成: 第{}次教学网登录成功", idx + 1);
                last_err.clear();
                break;
            }
            Ok(_) => { last_err = AUTH_EXPIRED.into(); }
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

    session.token = token;
    session.created_at = Utc::now();
    Ok(session)
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

pub fn fetch_with_cookie_redirect(cookie_file: &str, user_agent: &str, url: &str) -> Result<String, String> {
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
        "-o".to_string(),
        null_device().to_string(),
        "-w".to_string(),
        "%{url_effective}".to_string(),
        url.to_string(),
    ];

    run_curl_text(&args).and_then(|destination| {
        if let Ok(url) = reqwest::Url::parse(destination.trim()) {
            if url.host_str() == Some("iaaa.pku.edu.cn")
                || url.path().starts_with("/webapps/login/") {
                return Err(AUTH_EXPIRED.into());
            }
        }
        Ok(destination)
    })
}

pub fn download_token_in_session(login: &PkuLoginResult, url: &str) -> Result<Vec<u8>, String> {
    let _guard = login.lock_cookie();
    let cookie = login.cookie_file.to_string_lossy().to_string();
    let mut args = vec!["-sS".into(), "-k".into(), "-L".into(), "--http1.1".into(),
        "-A".into(), login.user_agent.clone(), "-b".into(), cookie.clone(), "-c".into(), cookie.clone()];
    if let Some(token) = read_token_from_cookie_file(&cookie) {
        args.extend(["-H".into(), format!("Cookie: _token={token}")]);
    }
    args.push(url.into());
    login.checked(run_curl_bytes(&args).and_then(|bytes| {
        if let Ok(text) = std::str::from_utf8(&bytes) { validate_page(text)?; }
        Ok(bytes)
    }))
}

/// Require the portal shell as well as absence of a login form. Empty courses are valid.
pub fn is_teaching_portal(page: &str) -> bool {
    ["courseListing", "courseList", "myCourses", "课程列表", "我的课程", "type=Course"]
        .iter().any(|marker| page.contains(marker))
}

fn read_token_from_cookie_file(path: &str) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file).lines() {
        let line = line.ok()?;
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(&line);
        if line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 7 && parts[5] == "_token" {
            return Some(parts[6].to_string());
        }
    }
    None
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    fn test_session() -> PkuLoginResult {
        let path = std::env::temp_dir().join(format!("pku-auth-test-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&path, "cookies").unwrap();
        PkuLoginResult::new(path, "test-agent".into(), String::new())
    }

    #[test]
    fn detects_login_forms_and_accepts_empty_portal() {
        assert!(validate_page("<input TYPE = 'PASSWORD' name='password'>").is_err());
        assert!(validate_page("<form id='form-pku-login'></form>").is_err());
        let empty = "<div id='courseListing'>目前没有课程</div>";
        assert!(validate_page(empty).is_ok());
        assert!(is_teaching_portal(empty));
        assert!(!is_teaching_portal("<html>服务器错误</html>"));
    }

    #[tokio::test]
    async fn authentication_retry_is_bounded_and_failure_evicts_new_session() {
        let cache = LoginCache::new(60);
        let mut login = cache.insert("user", test_session()).await;
        let mut calls = 0;
        let mut refreshes = 0;
        let result: Result<(), String> = session_operation_with_refresh(&mut login, |_| {
            calls += 1;
            Err(AUTH_EXPIRED.into())
        }, || {
            refreshes += 1;
            let cache = cache.clone();
            async move { Ok(cache.insert("user", test_session()).await) }
        }).await;
        assert!(result.is_err());
        assert_eq!((calls, refreshes), (2, 1));
        assert!(cache.get("user").await.is_none());
    }

    #[tokio::test]
    async fn network_failure_clears_cache_without_relogin() {
        let cache = LoginCache::new(60);
        let mut login = cache.insert("user", test_session()).await;
        let result: Result<(), String> = session_operation_with_refresh(&mut login,
            |_| Err("网络连接失败".into()),
            || async { panic!("ordinary errors must not trigger login") }).await;
        assert!(result.is_err());
        assert!(cache.get("user").await.is_none());
    }

    #[tokio::test]
    async fn running_operation_uses_expired_lease_without_refreshing() {
        let cache = LoginCache::new(60);
        let mut session = test_session();
        session.created_at = Utc::now() - chrono::Duration::minutes(61);
        let mut login = cache.insert("user", session).await;
        cache.cleanup_expired().await;
        let result = session_operation_with_refresh(&mut login,
            |session| Ok(session.cookie_file.is_file()),
            || async { panic!("running operation must not check TTL") }).await;
        assert_eq!(result, Ok(true));
    }

    #[tokio::test]
    async fn authentication_retry_can_succeed_with_new_session() {
        let cache = LoginCache::new(60);
        let mut login = cache.insert("user", test_session()).await;
        let old_path = login.cookie_file.clone();
        let mut calls = 0;
        let result = session_operation_with_refresh(&mut login, |_| {
            calls += 1;
            if calls == 1 { Err(AUTH_EXPIRED.into()) } else { Ok(42) }
        }, || async { Ok(cache.insert("user", test_session()).await) }).await;
        assert_eq!(result, Ok(42));
        assert!(!old_path.exists());
        assert_eq!(cache.get("user").await.unwrap().cookie_file, login.cookie_file);
    }

    #[tokio::test]
    async fn watching_page_cookies_reach_key_request_and_401_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for response in [
                "HTTP/1.1 200 OK\r\nSet-Cookie: login=yes; Path=/; HttpOnly\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 302 Found\r\nLocation: /player\r\nSet-Cookie: _token=watch-token; Path=/; HttpOnly\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\n0123456789abcdef",
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: 6\r\nConnection: close\r\n\r\ndenied",
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let len = stream.read(&mut buffer).unwrap();
                    assert!(len > 0);
                    request.extend_from_slice(&buffer[..len]);
                }
                requests.push(String::from_utf8(request).unwrap());
                stream.write_all(response.as_bytes()).unwrap();
            }
            requests
        });
        let dir = tempdir::TempDir::new("pku-session-test").unwrap();
        let login = PkuLoginResult::new(dir.path().join("cookies.txt"), "session-test-agent".into(), String::new());
        download_in_session(&login, &format!("{base}/login")).unwrap();
        let cache = LoginCache::new(60);
        let login = cache.insert("test-user", login).await;
        fetch_with_cookie_redirect(
            login.cookie_file.to_str().unwrap(), &login.user_agent, &format!("{base}/watch"),
        ).unwrap();
        assert_eq!(read_token_from_cookie_file(login.cookie_file.to_str().unwrap()).as_deref(), Some("watch-token"));
        assert_eq!(download_in_session(&login, &format!("{base}/key")).unwrap(), b"0123456789abcdef");
        assert!(cache.get("test-user").await.is_some());
        assert!(download_in_session(&login, &format!("{base}/denied")).unwrap_err().contains("401"));
        assert!(cache.get("test-user").await.is_none());
        assert!(login.cookie_file.is_file());
        let requests = server.join().unwrap();
        assert!(requests[1].contains("login=yes"));
        assert!(requests[3].contains("login=yes"));
        assert!(requests[3].contains("_token=watch-token"));
        assert!(requests.iter().all(|r| r.contains("session-test-agent")));
    }
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
        .args(["--fail", "--connect-timeout", "15", "--max-time", "60"])
        .args(args)
        .output()
        .map_err(|e| format!("调用 {} 失败: {}", curl_cmd, e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("curl 返回非 0: {}", stderr.trim()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub(crate) fn run_curl_bytes(args: &[String]) -> Result<Vec<u8>, String> {
    let curl_cmd = if cfg!(windows) { "curl.exe" } else { "curl" };
    let output = Command::new(curl_cmd)
        .args(["--fail", "--connect-timeout", "15"])
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
