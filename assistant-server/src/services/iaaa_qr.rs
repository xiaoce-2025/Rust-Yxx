//! Short-lived, browser-bound IAAA QR transactions. Upstream tickets never reach the browser.
use super::{login_cache::LoginCacheEntry, pkulogin};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};
use uuid::Uuid;

pub const LIFETIME: Duration = Duration::from_secs(183);
pub type Transactions = HashMap<String, Transaction>;

#[derive(Clone)]
pub struct Transaction {
    pub owner: Option<Uuid>,
    pub created: Instant,
    pub last_poll: Instant,
    pub session: LoginCacheEntry,
    pub active: Arc<AtomicBool>,
    pub polling: Arc<AtomicBool>,
}

pub enum Poll {
    Pending,
    Stopped(&'static str),
    Accepted(String),
}

fn form(pairs: &[(&str, &str)]) -> String {
    reqwest::Url::parse_with_params("https://iaaa.pku.edu.cn/", pairs)
        .unwrap()
        .query()
        .unwrap()
        .into()
}

fn request(
    session: &LoginCacheEntry,
    base: &str,
    path: &str,
    body: Option<&str>,
) -> Result<Vec<u8>, String> {
    let headers = vec![
        ("Referer".into(), format!("{base}oauth.jsp")),
        ("X-Requested-With".into(), "XMLHttpRequest".into()),
        (
            "Content-Type".into(),
            "application/x-www-form-urlencoded".into(),
        ),
        (
            "Origin".into(),
            reqwest::Url::parse(base)
                .map_err(|_| "无效的 IAAA 地址")?
                .origin()
                .ascii_serialization(),
        ),
    ];
    let response = pkulogin::request_in_session(session, &format!("{base}{path}"), &headers, body)?;
    if !(200..400).contains(&response.status) {
        return Err("IAAA 扫码服务暂不可用".into());
    }
    Ok(response.body)
}

pub fn start(owner: Option<Uuid>) -> Result<(Transaction, String), String> {
    start_at(owner, "https://iaaa.pku.edu.cn/iaaa/")
}

fn start_at(owner: Option<Uuid>, base: &str) -> Result<(Transaction, String), String> {
    let session = LoginCacheEntry::new(
        std::env::temp_dir().join(format!("iaaa_qr_{}.txt", Uuid::new_v4())),
        super::user_agents::choose("./user_agents.txt.gz")?,
    );
    request(
        &session,
        base,
        "oauth.jsp",
        Some(&form(&[
            ("appID", pkulogin::BLACKBOARD_APP_ID),
            ("appName", "1"),
            ("redirectUrl", pkulogin::BLACKBOARD_REDIR_URL),
        ])),
    )?;
    let query = form(&[
        ("userName", ""),
        ("_rand", &rand::random::<f64>().to_string()),
        ("appId", pkulogin::BLACKBOARD_APP_ID),
    ]);
    let bytes = request(&session, base, &format!("genQRCode.do?{query}"), None)?;
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else {
        return Err("IAAA 未返回有效二维码，请刷新重试".into());
    };
    if bytes.len() > 1024 * 1024 {
        return Err("IAAA 二维码响应过大".into());
    }
    let image = format!("data:{mime};base64,{}", STANDARD.encode(bytes));
    Ok((
        Transaction {
            owner,
            created: Instant::now(),
            last_poll: Instant::now(),
            session,
            active: Arc::new(AtomicBool::new(true)),
            polling: Arc::new(AtomicBool::new(false)),
        },
        image,
    ))
}

pub fn poll(transaction: &Transaction) -> Result<Poll, String> {
    poll_at(transaction, "https://iaaa.pku.edu.cn/iaaa/")
}

fn poll_at(transaction: &Transaction, base: &str) -> Result<Poll, String> {
    let bytes = request(
        &transaction.session,
        base,
        "oauthlogin4QRCode.do",
        Some(&form(&[
            ("appId", "PKUApp"),
            ("issuerAppId", "iaaa"),
            ("targetAppId", pkulogin::BLACKBOARD_APP_ID),
            ("redirectUrl", pkulogin::BLACKBOARD_REDIR_URL),
        ])),
    )?;
    let result: Value =
        serde_json::from_slice(&bytes).map_err(|_| "IAAA 扫码响应无效，请刷新二维码")?;
    parse_poll(&result)
}

fn parse_poll(result: &Value) -> Result<Poll, String> {
    if result["success"] == true {
        return result["token"]
            .as_str()
            .filter(|t| !t.trim().is_empty())
            .map(|t| Poll::Accepted(t.into()))
            .ok_or_else(|| "IAAA 未返回有效扫码票据，请刷新二维码".into());
    }
    if super::iaaa_login::login_limited(result) {
        return Ok(Poll::Stopped(super::iaaa_login::LOGIN_LIMIT_MESSAGE));
    }
    if result["isStop"] == "是" || result["errors"]["code"] == "E99" {
        return Ok(Poll::Stopped("二维码已失效，请刷新后重新扫码"));
    }
    match result["errors"]["code"].as_str() {
        Some("E10") => Ok(Poll::Pending),
        Some("E02") => Ok(Poll::Stopped(
            "IAAA 账号尚未激活，请先在官方认证页面完成激活",
        )),
        Some("E06") => Ok(Poll::Stopped(
            "请先在北京大学 App 完成手机令牌设置，再刷新二维码",
        )),
        _ if result["success"] == false
            && result.pointer("/errors/msg").and_then(Value::as_str) == Some("") =>
        {
            Ok(Poll::Pending)
        }
        _ => Err("扫码认证未通过，请刷新二维码后重试".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn qr_poll_requires_explicit_success_and_ticket_and_handles_stop_codes() {
        assert!(matches!(
            parse_poll(&json!({"success":false,"errors":{"code":"E10"}})).unwrap(),
            Poll::Pending
        ));
        assert!(
            matches!(parse_poll(&json!({"success":true,"token":"one-use"})).unwrap(), Poll::Accepted(t) if t == "one-use")
        );
        for result in [
            json!({}),
            json!({"success":true}),
            json!({"success":true,"token":" "}),
        ] {
            assert!(parse_poll(&result).is_err());
        }
        for result in [
            json!({"success":false,"isStop":"是"}),
            json!({"errors":{"code":"E99"}}),
            json!({"errors":{"code":"E02"}}),
            json!({"errors":{"code":"E06"}}),
            json!({"errors":{"msg":"登录次数过多 secret"}}),
        ] {
            assert!(
                matches!(parse_poll(&result).unwrap(), Poll::Stopped(message) if !message.contains("secret"))
            );
        }
    }

    #[tokio::test]
    async fn qr_generation_and_poll_use_one_cookie_jar_and_exact_blackboard_parameters() {
        use axum::{
            extract::{Form, Query},
            http::{header::SET_COOKIE, HeaderMap},
            routing::{get, post},
            Json, Router,
        };
        let app = Router::new()
            .route(
                "/oauth.jsp",
                post(|Form(fields): Form<HashMap<String, String>>| async move {
                    assert_eq!(fields["appID"], pkulogin::BLACKBOARD_APP_ID);
                    assert_eq!(fields["redirectUrl"], pkulogin::BLACKBOARD_REDIR_URL);
                    (
                        [(SET_COOKIE, "JSESSIONID=qr-test; Path=/; HttpOnly")],
                        "initialized",
                    )
                }),
            )
            .route(
                "/genQRCode.do",
                get(
                    |headers: HeaderMap, Query(fields): Query<HashMap<String, String>>| async move {
                        assert!(headers["cookie"]
                            .to_str()
                            .unwrap()
                            .contains("JSESSIONID=qr-test"));
                        assert_eq!(fields["appId"], "blackboard");
                        assert_eq!(fields["userName"], "");
                        assert!(fields["_rand"].parse::<f64>().is_ok());
                        b"\x89PNG\r\n\x1a\nfixture".to_vec()
                    },
                ),
            )
            .route(
                "/oauthlogin4QRCode.do",
                post(
                    |headers: HeaderMap, Form(fields): Form<HashMap<String, String>>| async move {
                        assert!(headers["cookie"]
                            .to_str()
                            .unwrap()
                            .contains("JSESSIONID=qr-test"));
                        assert_eq!(fields.len(), 4);
                        assert_eq!(fields["appId"], "PKUApp");
                        assert_eq!(fields["issuerAppId"], "iaaa");
                        assert_eq!(fields["targetAppId"], "blackboard");
                        assert_eq!(fields["redirectUrl"], pkulogin::BLACKBOARD_REDIR_URL);
                        Json(json!({"success":true,"token":"fixture-ticket"}))
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::task::spawn_blocking(move || {
            let (transaction, image) = start_at(None, &base).unwrap();
            assert!(image.starts_with("data:image/png;base64,"));
            assert!(matches!(poll_at(&transaction, &base).unwrap(), Poll::Accepted(token) if token == "fixture-ticket"));
            let cookie_file = transaction.session.cookie_file.clone();
            assert!(cookie_file.exists());
            drop(transaction);
            assert!(!cookie_file.exists());
        }).await.unwrap();
        server.abort();
    }
}
