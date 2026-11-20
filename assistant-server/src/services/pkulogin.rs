use std::process::Command;

use chrono::Utc;
use serde_json::Value;

use crate::services::login_cache::LoginCache;
use super::iaaa_login::{PublicKey, PUBLIC_KEY_URL};

const IAAA_OAUTH_LOGIN_URL: &str = "https://iaaa.pku.edu.cn/iaaa/oauthlogin.do";
pub(crate) const BLACKBOARD_APP_ID: &str = "blackboard";
// Match the redirectUrl declared by Blackboard's IAAA form.
pub(crate) const BLACKBOARD_REDIR_URL: &str =
    "http://course.pku.edu.cn/webapps/bb-sso-BBLEARN/execute/authValidate/campusLogin";
// The browser upgrades that URL locally through HSTS before making a network request.
const BLACKBOARD_CALLBACK_URL: &str =
    "https://course.pku.edu.cn/webapps/bb-sso-BBLEARN/execute/authValidate/campusLogin";

fn campus_login_url(token: &str) -> String {
    let mut url = reqwest::Url::parse(BLACKBOARD_CALLBACK_URL).expect("constant callback URL");
    url.query_pairs_mut().append_pair("_rand", &format!("{:.16}", rand::random::<f64>()))
        .append_pair("token", token);
    url.into()
}

/// Consume a QR ticket once, then derive identity from the authenticated server session.
pub(crate) fn complete_qr_login(session: &PkuLoginResult, token: &str) -> Result<String, String> {
    let page = fetch_with_cookie(&session.cookie_file.to_string_lossy(), &session.user_agent, &campus_login_url(token))?;
    validate_page(&page)?;
    let portal = fetch_page_in_session(session,
        "https://course.pku.edu.cn/webapps/portal/execute/tabs/tabAction?tab_tab_group_id=_1_1")?;
    if !is_teaching_portal(&portal) { return Err("教学网登录后未返回可识别的门户页面".into()); }
    let me = request_in_session(session, "https://course.pku.edu.cn/learn/api/public/v1/users/me", &[], None)?;
    if me.status != 200 { return Err("无法确认扫码账号，请重新扫码".into()); }
    let identity: Value = serde_json::from_slice(&me.body).map_err(|_| "扫码账号响应无效")?;
    let username = qr_username(&identity)?;
    discard_iaaa_cookies(&session.cookie_file)?;
    Ok(username)
}

fn qr_username(identity: &Value) -> Result<String, String> {
    identity.get("userName").and_then(Value::as_str)
        .filter(|name| name.len() == 10 && name.bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_owned).ok_or_else(|| "请使用 10 位学号对应的北大账号扫码".into())
}

/// Keep only the established teaching/replay environment; IAAA's own cookies are not cached.
fn discard_iaaa_cookies(path:&std::path::Path)->Result<(),String> {
    let content=std::fs::read_to_string(path).map_err(|e|format!("读取教学网 Cookie jar 失败: {e}"))?;
    let lines:Vec<_>=content.lines().filter(|line| {
        let row=line.strip_prefix("#HttpOnly_").unwrap_or(line);
        let domain=row.split('\t').next().unwrap_or_default().trim_start_matches('.');
        !domain.eq_ignore_ascii_case("iaaa.pku.edu.cn")
    }).collect();
    std::fs::write(path,format!("{}\n",lines.join("\n"))).map_err(|e|format!("保存教学网 Cookie jar 失败: {e}"))
}

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
        || async {
            if password.is_empty() { return Err(super::teaching_session::LOGIN_REQUIRED.into()); }
            login_iaaa_and_get_cookies_with_cache(username, password, cache).await
        }).await
}

pub(super) async fn session_operation_with_refresh<T, F, Fut>(
    login: &mut PkuLoginResult,
    mut operation: impl FnMut(&PkuLoginResult) -> Result<T, String>,
    refresh: F,
) -> Result<T, String>
where F: FnOnce() -> Fut, Fut: std::future::Future<Output = Result<PkuLoginResult, String>> {
    let result = login.checked(operation(login));
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
    cached_teaching_login(username, login_cache,
        || async { login_iaaa(username, LoginPassword::Submitted(password.to_owned())).await.map(|(session, _, _)| session) }).await
}

/// A factory encompasses the complete IAAA -> single callback -> portal verification transaction.
/// Only its successful teaching session can enter the cache; failed callbacks leave no reusable ticket.
pub(super) async fn cached_teaching_login<F,Fut>(username:&str, cache:&LoginCache, login:F) -> Result<PkuLoginResult,String>
where F:FnOnce()->Fut, Fut:std::future::Future<Output=Result<PkuLoginResult,String>> {
    let lock=cache.login_lock(username).await;
    let _guard=lock.lock().await;
    if let Some(entry)=cache.get(username).await {
        tracing::info!(user_agent=%entry.user_agent,"复用教学网缓存会话，使用 User-Agent（完整值）");
        return Ok(entry);
    }
    let session=login().await?;
    Ok(cache.insert(username,session).await)
}

pub(crate) enum LoginPassword {
    Plaintext(String),
    Submitted(String),
    Bound { ciphertext: String, key: Option<String>, version: Option<String> },
}

pub(crate) async fn login_iaaa(username: &str, password: LoginPassword) -> Result<(PkuLoginResult, PublicKey, String), String> {
    // The curl transaction is blocking; do not stall the async page/API executor.
    let username = username.to_owned();
    tokio::task::spawn_blocking(move || {
        let ua = super::user_agents::choose("./user_agents.txt.gz")?;
        // IAAA is rate limited: submit credentials once and return any failure directly.
        login_with_user_agent(&username, password, ua)
    }).await.map_err(|_| "教学网登录任务失败".to_string())?
}

fn login_with_user_agent(username: &str, password: LoginPassword, user_agent: String) -> Result<(PkuLoginResult, PublicKey, String), String> {
    let start_time = chrono::Utc::now();
    tracing::info!("IAAA登录流程开始 - 时间: {}", start_time);


    let cookie_file = std::env::temp_dir().join(format!(
        "pku_cookie_{}_{}.txt",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut session = PkuLoginResult::new(cookie_file.clone(), user_agent.clone());
    let cookie_file_str = cookie_file.to_string_lossy().to_string();
    tracing::debug!("步骤2: 创建临时Cookie文件 - {}", cookie_file_str);

    // Establish the same IAAA session as the browser before submitting credentials.
    fetch_with_cookie(&cookie_file_str, &user_agent,
        "https://course.pku.edu.cn/webapps/bb-sso-BBLEARN/login.html")?;
    run_curl_text(&["-sS".into(), "-L".into(), "-A".into(), user_agent.clone(),
        "-b".into(), cookie_file_str.clone(), "-c".into(), cookie_file_str.clone(),
        "-e".into(), "https://course.pku.edu.cn/".into(),
        "--data-raw".into(), format!("appID=blackboard&appName=1&redirectUrl={}", urlencoding::encode(BLACKBOARD_REDIR_URL)),
        "https://iaaa.pku.edu.cn/iaaa/oauth.jsp".into()])?;

    // Step 1: IAAA oauthlogin -> token
    let key_response = run_curl_text(&[
        "-sS".into(), "--max-time".into(), "15".into(), "-A".into(), user_agent.clone(),
        "-b".into(), cookie_file_str.clone(), "-c".into(), cookie_file_str.clone(),
        PUBLIC_KEY_URL.into(),
    ])?;
    let key = PublicKey::parse(&serde_json::from_str(&key_response).map_err(|_| "IAAA 公钥响应无效")?)?;
    let encrypted = match password {
        LoginPassword::Plaintext(password) => super::app_replay::encrypt_binding_password(&key.key, &password)?,
        LoginPassword::Submitted(password) => password,
        LoginPassword::Bound { ciphertext, key: stored_key, version } => {
            key.check_binding(stored_key.as_deref(), version.as_deref())?;
            ciphertext
        }
    };
    let form_body = format!(
        "appid={}&userName={}&password={}&keyVersion={}&randCode=&smsCode=&otpCode=&remTrustChk=false&redirUrl={}",
        urlencoding::encode(BLACKBOARD_APP_ID),
        urlencoding::encode(username),
        urlencoding::encode(&encrypted),
        urlencoding::encode(&key.version),
        urlencoding::encode(BLACKBOARD_REDIR_URL)
    );
    tracing::debug!("步骤3: 准备OAuth登录请求 - body长度: {}", form_body.len());

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
        "-e".into(), "https://iaaa.pku.edu.cn/iaaa/oauth.jsp".into(),
        "-H".into(), "Origin: https://iaaa.pku.edu.cn".into(),
        "-H".into(), "X-Requested-With: XMLHttpRequest".into(),
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

    // This IAAA ticket is ephemeral and single-use. Never retry a callback with it,
    // and never retain it in the teaching-session cache, even if IAAA itself succeeded.
    let campus_url=campus_login_url(&token);
    tracing::info!(user_agent=%user_agent,"步骤7: 使用本次新取得的 IAAA token，执行一次 HTTPS 教学网回调");
    let callback=(|| {
        let page=fetch_with_cookie(&cookie_file_str,&user_agent,&campus_url)?;
        validate_page(&page)?;
        let portal=fetch_page_in_session(&session,
            "https://course.pku.edu.cn/webapps/portal/execute/tabs/tabAction?tab_tab_group_id=_1_1")?;
        if !is_teaching_portal(&portal) {return Err("教学网登录后未返回可识别的门户页面".to_string());}
        Ok(())
    })();
    if let Err(error)=callback {
        tracing::error!(user_agent=%user_agent,error=%error,"教学网回调失败；本次 IAAA token 作废，下次必须重新进行 IAAA 登录");
        return Err(format!("教学网 token 登录失败: {error}"));
    }
    tracing::info!("步骤7完成: 教学网回调及门户验证成功，可以缓存教学网 Cookie 会话");

    let end_time = chrono::Utc::now();
    let duration = end_time - start_time;
    tracing::info!("IAAA登录流程完成 - 总耗时: {}ms", duration.num_milliseconds());

    discard_iaaa_cookies(&session.cookie_file)?;
    session.created_at = Utc::now();
    Ok((session, key, encrypted))
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

    #[test]
    fn qr_identity_must_be_a_server_returned_ten_digit_username() {
        assert_eq!(qr_username(&serde_json::json!({"userName":"2400012345"})).unwrap(), "2400012345");
        for value in [serde_json::json!({}), serde_json::json!({"userName":"admin"}),
            serde_json::json!({"studentId":"2400012345"}), serde_json::json!({"userName":"2400012345@evil.test"})] {
            assert!(qr_username(&value).is_err());
        }
    }
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    fn test_session() -> PkuLoginResult {
        let path = std::env::temp_dir().join(format!("pku-auth-test-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&path, "cookies").unwrap();
        PkuLoginResult::new(path, "Windows test-agent".into())
    }

    #[tokio::test]
    async fn failed_teaching_callback_is_not_cached_and_next_attempt_restarts_iaaa() {
        let cache=LoginCache::new();
        let mut iaaa_calls=0;
        let first:Result<PkuLoginResult,String>=cached_teaching_login("ticket-test",&cache,|| {
            iaaa_calls+=1;
            async {Err("IAAA已成功，但教学网回调失败".into())}
        }).await;
        assert!(first.is_err());
        assert_eq!(iaaa_calls, 1);
        assert!(cache.get("ticket-test").await.is_none());
        let dir=tempdir::TempDir::new("teaching-cache").unwrap();
        let cookie=dir.path().join("cookies.txt");
        std::fs::write(&cookie,"teaching-cookie").unwrap();
        let expected=cookie.clone();
        let session=cached_teaching_login("ticket-test",&cache,|| {
            iaaa_calls+=1;
            async {Ok(PkuLoginResult::new(cookie,format!("Windows successful-ua-{}",uuid::Uuid::new_v4())))}
        }).await.unwrap();
        assert_eq!(iaaa_calls,2);
        assert_eq!(session.cookie_file,expected);
        let reused=cached_teaching_login("ticket-test",&cache,||async {
            panic!("verified teaching cookies must be reused without obtaining an IAAA ticket")
        }).await.unwrap();
        assert_eq!(reused.cookie_file,expected);
    }

    #[tokio::test]
    async fn old_cached_session_is_reused_regardless_of_ua_platform() {
        let cache = LoginCache::new();
        let dir = tempdir::TempDir::new("ua-cache").unwrap();
        let cookie = dir.path().join("cookies.txt");
        std::fs::write(&cookie, "teaching-cookie").unwrap();
        let mut session = PkuLoginResult::new(cookie.clone(), "Android mobile".into());
        session.created_at = Utc::now() - chrono::Duration::days(365);
        cache.insert("ua-test", session).await;
        let entry = cached_teaching_login("ua-test", &cache, || async {
            panic!("UA platform must not trigger another IAAA login")
        }).await.unwrap();
        assert_eq!(entry.cookie_file, cookie);
        assert_eq!(entry.user_agent, "Android mobile");
    }

    #[test]
    fn cached_cookie_jar_excludes_iaaa_and_preserves_teaching_and_replay_cookies() {
        let dir=tempdir::TempDir::new("iaaa-cookie-filter").unwrap();let path=dir.path().join("cookies.txt");
        std::fs::write(&path,concat!("# Netscape HTTP Cookie File\n",
            "#HttpOnly_iaaa.pku.edu.cn\tFALSE\t/\tTRUE\t0\tJSESSIONID\tiaaa-cookie\n",
            ".iaaa.pku.edu.cn\tTRUE\t/\tTRUE\t0\tlogin\tiaaa-cookie\n",
            "#HttpOnly_course.pku.edu.cn\tFALSE\t/\tTRUE\t0\tJSESSIONID\tteaching-cookie\n",
            ".pku.edu.cn\tTRUE\t/\tTRUE\t0\t_token\treplay-cookie\n")).unwrap();
        discard_iaaa_cookies(&path).unwrap();
        let cached=std::fs::read_to_string(&path).unwrap();
        assert!(!cached.contains("iaaa.pku.edu.cn"));
        assert!(cached.contains("#HttpOnly_course.pku.edu.cn"));
        assert!(cached.contains("teaching-cookie"));
        assert!(cached.contains("replay-cookie"));
    }

    #[test]
    fn callback_uses_https_before_sending_token_and_preserves_form_redirect() {
        // Reserved characters must survive URL construction without changing the token.
        let token = "sample+token/with=&reserved";
        let callback = reqwest::Url::parse(&campus_login_url(token)).unwrap();
        assert_eq!(callback.scheme(), "https");
        assert_eq!(callback.host_str(), Some("course.pku.edu.cn"));
        assert_eq!(callback.path(), "/webapps/bb-sso-BBLEARN/execute/authValidate/campusLogin");
        let query: std::collections::HashMap<_,_> = callback.query_pairs().into_owned().collect();
        assert_eq!(query.get("token").unwrap(), token);
        assert!(query.contains_key("_rand"));
        assert_eq!(reqwest::Url::parse(BLACKBOARD_REDIR_URL).unwrap().scheme(), "http");
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
        let cache = LoginCache::new();
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
        let cache = LoginCache::new();
        let mut login = cache.insert("user", test_session()).await;
        let result: Result<(), String> = session_operation_with_refresh(&mut login,
            |_| Err("网络连接失败".into()),
            || async { panic!("ordinary errors must not trigger login") }).await;
        assert!(result.is_err());
        assert!(cache.get("user").await.is_none());
    }

    #[tokio::test]
    async fn temporary_session_expiry_never_attempts_login_with_empty_credentials() {
        let cache = LoginCache::new();
        let mut login = cache.insert("temporary-user", test_session()).await;
        let result: Result<(), String> = session_operation(&mut login, "", "", &cache,
            |_| Err(AUTH_EXPIRED.into())).await;
        assert_eq!(result.unwrap_err(), super::super::teaching_session::LOGIN_REQUIRED);
        assert!(cache.get("temporary-user").await.is_none());
    }

    #[tokio::test]
    async fn running_operation_reuses_old_session_without_refreshing() {
        let cache = LoginCache::new();
        let mut session = test_session();
        session.created_at = Utc::now() - chrono::Duration::days(365);
        let mut login = cache.insert("user", session).await;
        cache.cleanup_missing_cookies().await;
        let result = session_operation_with_refresh(&mut login,
            |session| Ok(session.cookie_file.is_file()),
            || async { panic!("running operation must not check TTL") }).await;
        assert_eq!(result, Ok(true));
    }

    #[tokio::test]
    async fn authentication_retry_can_succeed_with_new_session() {
        let cache = LoginCache::new();
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
                let request = String::from_utf8(request).unwrap();
                requests.push(request);
                stream.write_all(response.as_bytes()).unwrap();
            }
            requests
        });
        let dir = tempdir::TempDir::new("pku-session-test").unwrap();
        let login = PkuLoginResult::new(dir.path().join("cookies.txt"), "Windows session-test-agent".into());
        download_in_session(&login, &format!("{base}/login")).unwrap();
        let cache = LoginCache::new();
        let login = cache.insert("test-user", login).await;
        fetch_with_cookie_redirect(
            login.cookie_file.to_str().unwrap(), &login.user_agent, &format!("{base}/watch"),
        ).unwrap();
        assert_eq!(read_token_from_cookie_file(login.cookie_file.to_str().unwrap()).as_deref(), Some("watch-token"));
        assert_eq!(download_playback_key_in_session(&login, &format!("{base}/key")).unwrap(), b"0123456789abcdef");
        assert!(cache.get("test-user").await.is_some());
        assert!(download_in_session(&login, &format!("{base}/denied")).unwrap_err().contains("401"));
        assert!(cache.get("test-user").await.is_none());
        assert!(login.cookie_file.is_file());
        let requests = server.join().unwrap();
        assert!(requests[1].contains("login=yes"));
        assert!(requests[3].contains("login=yes"));
        assert!(requests[3].contains("_token=watch-token"));
        assert!(requests[3].contains("Origin: https://onlineroomse.pku.edu.cn"));
        assert!(requests[3].contains("Referer: https://onlineroomse.pku.edu.cn/"));
        assert!(requests.iter().all(|r| r.contains("session-test-agent")));
    }
}

fn parse_token(body: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| format!("解析 IAAA 登录响应 JSON 失败: {e}"))?;

    if super::iaaa_login::login_limited(&value) {
        return Err(super::iaaa_login::LOGIN_LIMIT_MESSAGE.into());
    }

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
        .ok_or_else(|| "IAAA 登录响应缺少 token 字段".to_string())?;

    if token.trim().is_empty() {
        return Err("IAAA 登录 token 为空".to_string());
    }

    Ok(token.trim().to_string())
}

fn run_curl_text(args: &[String]) -> Result<String, String> {
    run_curl_bytes(args).map(|bytes|String::from_utf8_lossy(&bytes).into_owned())
}

fn curl_user_agent(args: &[String]) -> Option<&str> {
    args.windows(2).find(|pair|pair[0] == "-A" || pair[0] == "--user-agent").map(|pair|pair[1].as_str())
}

pub(crate) fn run_curl_bytes(args: &[String]) -> Result<Vec<u8>, String> {
    if let Some(ua)=curl_user_agent(args) {
        if !super::user_agents::valid_header(ua) {
            return Err("无效的 User-Agent".into());
        }
    }
    let curl_cmd = if cfg!(windows) { "curl.exe" } else { "curl" };
    let output = Command::new(curl_cmd)
        .args(["--fail", "--connect-timeout", "15", "--max-time", "60"])
        .args(args).output().map_err(|error| {
            format!("调用 {curl_cmd} 失败: {error}")
        })?;
    if !output.status.success() {
        if let Some(ua)=curl_user_agent(args) {
            tracing::warn!(user_agent=%ua,curl_exit_code=?output.status.code(),"curl 请求失败，使用的完整 User-Agent");
        }
        let stderr=String::from_utf8_lossy(&output.stderr);
        return Err(format!("curl 返回非 0: {}",stderr.trim()));
    }
    Ok(output.stdout)
}

pub(crate) struct TeachingResponse {
    pub status: u16,
    pub location: Option<String>,
    pub body: Vec<u8>,
}

/// Structured Blackboard requests use the replay's curl runner, UA and locked
/// cookie jar. Redirects are left to the caller so grading credentials never
/// follow an external redirect and a POST is never implicitly replayed.
pub(crate) fn request_in_session(
    login: &PkuLoginResult,
    url: &str,
    headers: &[(String, String)],
    form: Option<&str>,
) -> Result<TeachingResponse, String> {
    use std::io::Read;
    let _guard = login.lock_cookie();
    let dir = tempdir::TempDir::new("teaching-request")
        .map_err(|_| "无法创建教学网请求临时目录")?;
    let body_path = dir.path().join("body");
    let header_path = dir.path().join("headers");
    let cookie = login.cookie_file.to_string_lossy().to_string();
    let mut args = vec![
        "-sS".into(), "-k".into(), "--no-fail".into(),
        "-A".into(), login.user_agent.clone(),
        "-b".into(), cookie.clone(), "-c".into(), cookie,
        "--max-filesize".into(), (32 * 1024 * 1024).to_string(),
        "--output".into(), body_path.to_string_lossy().into_owned(),
        "--dump-header".into(), header_path.to_string_lossy().into_owned(),
        "--write-out".into(), "%{http_code}".into(),
    ];
    #[cfg(test)]
    if reqwest::Url::parse(url).ok().is_some_and(|u| matches!(u.host_str(), Some("127.0.0.1" | "localhost"))) {
        args.extend(["--noproxy".into(), "*".into()]);
    }
    for (name, value) in headers {
        args.extend(["-H".into(), format!("{name}: {value}")]);
    }
    if let Some(form) = form {
        let path = dir.path().join("form");
        std::fs::write(&path, form).map_err(|_| "无法准备教学网评分请求")?;
        // Keep feedback and nonce out of the process command line.
        args.extend(["--data-binary".into(), format!("@{}", path.display())]);
    }
    args.push(url.into());
    let metadata = run_curl_bytes(&args)?;
    let status = std::str::from_utf8(&metadata).ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .filter(|s| (100..600).contains(s))
        .ok_or("无法识别教学网 HTTP 状态")?;
    let headers = std::fs::read_to_string(header_path).map_err(|_| "无法读取教学网响应头")?;
    // Ignore proxy CONNECT and interim response headers.
    let final_headers = headers.rsplit("HTTP/").next().unwrap_or_default();
    let location = final_headers.lines().filter_map(|l| l.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("location"))
        .map(|(_, value)| value.trim().to_owned());
    let mut body = Vec::new();
    std::fs::File::open(body_path).map_err(|_| "无法读取教学网响应")?
        .take(32 * 1024 * 1024 + 1).read_to_end(&mut body)
        .map_err(|_| "读取教学网响应失败")?;
    if body.len() > 32 * 1024 * 1024 {
        return Err("教学网响应超过 32 MiB，请直接在教学网下载大附件".into());
    }
    Ok(TeachingResponse { status, location, body })
}


pub fn download_playback_key_in_session(login: &PkuLoginResult, url: &str) -> Result<Vec<u8>, String> {
    let _guard = login.lock_cookie();
    let cookie = login.cookie_file.to_string_lossy().to_string();
    login.checked(run_curl_bytes(&["-sS".into(), "-L".into(), "-k".into(),
        "-A".into(),login.user_agent.clone(), "-b".into(),cookie.clone(),"-c".into(),cookie,
        "-H".into(),"Origin: https://onlineroomse.pku.edu.cn".into(),
        "-e".into(),"https://onlineroomse.pku.edu.cn/".into(), url.into()]).and_then(|bytes| {
            if let Ok(text) = std::str::from_utf8(&bytes) { validate_page(text)?; }
            Ok(bytes)
        }))
}
