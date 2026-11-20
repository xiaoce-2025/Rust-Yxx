//! Fresh IAAA verification for signing in to the assistant. Never follows the teaching callback.
use reqwest::redirect::Policy;
use serde_json::Value;
use std::time::Duration;

use crate::error::ApiError;

const IAAA: &str = "https://iaaa.pku.edu.cn/iaaa/";
const REDIRECT: &str = "http://course.pku.edu.cn/webapps/bb-sso-BBLEARN/execute/authValidate/campusLogin";

pub(crate) const PUBLIC_KEY_URL: &str = "https://iaaa.pku.edu.cn/iaaa/getPublicKey.do";
pub(crate) const BINDING_EXPIRED: &str = "IAAA 公钥已更新，原绑定已失效，请重新绑定";

pub(crate) struct PublicKey {
    pub key: String,
    pub version: String,
}

impl PublicKey {
    pub fn parse(response: &Value) -> Result<Self, String> {
        let key = response.get("key").and_then(Value::as_str)
            .filter(|key| response["success"] == true && !key.trim().is_empty())
            .ok_or("无法获取 IAAA 公钥，请稍后重试")?;
        Ok(Self { key: key.to_owned(), version: key_version(response)? })
    }

    pub async fn fetch() -> Result<Self, String> {
        let response = reqwest::Client::new().get(PUBLIC_KEY_URL)
            .timeout(Duration::from_secs(10)).send().await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|_| "获取 IAAA 公钥失败，请稍后重试")?;
        Self::parse(&response.json().await.map_err(|_| "IAAA 公钥响应无效，请稍后重试")?)
    }

    pub fn check_binding(&self, key: Option<&str>, version: Option<&str>) -> Result<(), String> {
        // Older records have only the PEM. Whitespace differences do not change the RSA key.
        if key.is_some_and(|key| key.split_whitespace().flat_map(str::chars)
            .eq(self.key.split_whitespace().flat_map(str::chars)))
            && version.is_none_or(|version| version == self.version.as_str()) {
            Ok(())
        } else {
            Err(BINDING_EXPIRED.into())
        }
    }
}

/// Preserve the upstream key version exactly, including numeric versions and zero.
pub(crate) fn key_version(key: &Value) -> Result<String, String> {
    match key.get("version") {
        Some(Value::String(version)) if !version.trim().is_empty() => Ok(version.clone()),
        Some(Value::Number(version)) => Ok(version.to_string()),
        _ => Err("IAAA 未返回有效公钥版本，请稍后重试".into()),
    }
}

pub const LOGIN_LIMIT_MESSAGE: &str = "IAAA 登录次数过多，请稍后再试";

pub(super) fn login_limited(result: &Value) -> bool {
    result.pointer("/errors/msg").and_then(Value::as_str)
        .is_some_and(|message| message.contains("次数过多"))
}

pub async fn verify(username: &str, password: &str) -> Result<(), ApiError> {
    // An isolated cookie jar and a fresh request are essential: cached teaching sessions
    // must never be accepted as proof that the submitted password is correct.
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .redirect(Policy::none())
        .timeout(Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/130.0.0.0 Safari/537.36")
        .build().map_err(|source| upstream_error("创建认证客户端", source))?;
    tracing::info!(host="iaaa.pku.edu.cn", stage="初始化认证会话", "IAAA authentication started");
    client.post(format!("{IAAA}oauth.jsp"))
        .form(&[("appID", "blackboard"), ("appName", "1"), ("redirectUrl", REDIRECT)])
        .send().await.and_then(reqwest::Response::error_for_status)
        .map_err(|source| upstream_error("初始化认证会话（oauth.jsp）", source))?;
    tracing::debug!(host="iaaa.pku.edu.cn", stage="获取公钥", "IAAA session initialized");
    let key: Value = client.get(format!("{IAAA}getPublicKey.do"))
        .send().await.and_then(reqwest::Response::error_for_status)
        .map_err(|source| upstream_error("获取公钥（getPublicKey.do）", source))?
        .json().await.map_err(|source| upstream_error("解析公钥响应", source))?;
    let key = PublicKey::parse(&key).map_err(ApiError::Internal)?;
    let encrypted = super::app_replay::encrypt_binding_password(&key.key, password)
        .map_err(|_| ApiError::Internal("IAAA 密码加密失败，请稍后重试".into()))?;
    tracing::debug!(host="iaaa.pku.edu.cn", stage="校验凭据", "IAAA public key loaded; submitting encrypted credentials");
    let result: Value = client.post(format!("{IAAA}oauthlogin.do"))
        .header("Referer", format!("{IAAA}oauth.jsp"))
        .header("Origin", "https://iaaa.pku.edu.cn")
        .header("X-Requested-With", "XMLHttpRequest")
        .form(&[("appid", "blackboard"), ("userName", username), ("password", encrypted.as_str()),
            ("keyVersion", key.version.as_str()), ("randCode", ""), ("smsCode", ""), ("otpCode", ""),
            ("remTrustChk", "false"), ("redirUrl", REDIRECT)])
        .send().await.and_then(reqwest::Response::error_for_status)
        .map_err(|source| upstream_error("校验账号密码（oauthlogin.do）", source))?
        .json().await.map_err(|source| upstream_error("解析登录响应", source))?;
    verify_response(&result)?;
    tracing::info!(host="iaaa.pku.edu.cn", "IAAA authentication succeeded; token discarded");
    Ok(())
}

fn upstream_error(stage: &'static str, source: reqwest::Error) -> ApiError {
    let http_status = source.status().map(|status| status.as_u16());
    let timeout = source.is_timeout();
    let connect = source.is_connect();
    let decode = source.is_decode();
    let reason = if timeout {
        "请求超时".to_string()
    } else if let Some(status) = http_status {
        format!("认证服务返回 HTTP {status}")
    } else if connect {
        "连接建立失败（请检查 DNS、TCP 连接或 TLS 证书，具体原因见服务端日志）".to_string()
    } else if decode {
        "响应格式无效，无法解析 JSON".to_string()
    } else if source.is_body() {
        "读取响应失败，连接可能已中断".to_string()
    } else if source.is_builder() {
        "HTTP 请求或客户端配置无效".to_string()
    } else {
        "未取得有效响应，具体原因见服务端日志".to_string()
    };
    // Debug retains the transport's source chain. Remove request URLs; never log
    // passwords, request/response bodies, headers, cookies, public keys or tokens.
    let source = source.without_url();
    tracing::error!(host="iaaa.pku.edu.cn", stage, http_status, timeout, connect, decode,
        cause=?source, "IAAA upstream request failed");
    ApiError::Internal(format!("IAAA {stage}失败：{reason}"))
}

fn verify_response(result: &Value) -> Result<(), ApiError> {
    if result.get("success").and_then(Value::as_bool) == Some(true)
        && result.get("token").and_then(Value::as_str).is_some_and(|t| !t.trim().is_empty()) {
        // The one-use upstream token is deliberately discarded, never returned or stored.
        return Ok(());
    }
    if login_limited(result) {
        return Err(ApiError::BadRequest(LOGIN_LIMIT_MESSAGE.into()));
    }
    let code = result.pointer("/errors/code").and_then(Value::as_str)
        .filter(|code| code.len() <= 64 && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
    let success = result.get("success").and_then(Value::as_bool);
    let token_present = result.get("token").and_then(Value::as_str).is_some_and(|t| !t.trim().is_empty());
    let captcha_required = result.get("showCode").and_then(Value::as_bool).unwrap_or(false);
    let reason = match code {
        Some("E01") => "IAAA 拒绝了账号或密码，请使用北京大学统一身份认证密码（与教学网一致），而非严小希账户密码",
        Some("E02") => "IAAA 账号尚未激活，请先在北京大学统一身份认证页面完成激活",
        Some("E03") => "IAAA 要求图形验证码或验证码校验失败，当前登录页面暂不支持图形验证码",
        Some("E04") => "IAAA 短信验证码校验失败，当前登录页面暂不支持短信验证",
        Some("E05") => "IAAA 手机令牌校验失败，当前登录页面暂不支持手机令牌验证",
        _ => "IAAA 认证未通过或未返回有效 token，请稍后重试",
    };
    // Only known, fixed messages are surfaced; never copy arbitrary upstream data
    // into logs or the browser (the response may also contain authentication secrets).
    tracing::warn!(host="iaaa.pku.edu.cn", stage="校验登录响应", iaaa_code=code,
        success=?success, token_present, captcha_required, reason,
        "IAAA rejected authentication or returned no usable token");
    let mut message = format!("{}：{reason}", code.unwrap_or("IAAA"));
    if captcha_required && code != Some("E03") {
        message.push_str("；IAAA 同时要求图形验证码，当前登录页面暂不支持，请勿连续重复尝试");
    }
    Err(ApiError::BadRequest(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn public_key_version_preserves_strings_and_numbers_and_rejects_missing_values() {
        assert_eq!(key_version(&json!({"version":"001"})).unwrap(), "001");
        assert_eq!(key_version(&json!({"version":0})).unwrap(), "0");
        assert_eq!(key_version(&json!({"version":20261113})).unwrap(), "20261113");
        for key in [json!({}), json!({"version":null}), json!({"version":" "}), json!({"version":true})] {
            assert!(key_version(&key).is_err());
        }
    }

    #[test]
    fn binding_requires_the_same_pem_and_any_recorded_version() {
        let key = PublicKey::parse(&json!({"success": true, "key":"PEM\nbody", "version":"202512"})).unwrap();
        assert!(key.check_binding(Some("PEM\r\nbo\ndy"), Some("202512")).is_ok());
        assert!(key.check_binding(Some("PEM\nbody"), None).is_ok());
        for (pem, version) in [
            (Some("PEM\nbody"), Some("202601")),
            (Some("PEM\nrotated"), Some("202512")),
            (None, None),
        ] {
            assert_eq!(key.check_binding(pem, version).unwrap_err(), BINDING_EXPIRED);
        }
        assert!(PublicKey::parse(&json!({"success":false, "key":"PEM", "version":"202512"})).is_err());
    }

    #[test]
    fn login_limit_takes_precedence_over_credential_error_without_echoing_upstream_secrets() {
        for code in ["E01", "E99"] {
            let result = json!({"success":false,"errors":{"code":code,"msg":"登录次数过多 secret"}});
            let error = verify_response(&result).unwrap_err().to_string();
            assert!(error.contains(LOGIN_LIMIT_MESSAGE));
            assert!(!error.contains("secret"));
        }
        assert!(!login_limited(&json!({"errors":{"msg":null}})));
    }

    #[test]
    fn requires_explicit_success_and_nonempty_token() {
        for value in [json!({}), json!({"success":true}), json!({"success":true,"token":"  "}),
            json!({"token":"ticket"}), json!({"success":false,"token":"ticket"}),
            json!({"success":false,"errors":{"msg":"sensitive upstream detail"}})] {
            assert!(verify_response(&value).is_err());
        }
        assert!(verify_response(&json!({"success":true,"token":"ticket"})).is_ok());
    }

    #[tokio::test]
    async fn upstream_decode_error_identifies_stage_without_disclosing_response() {
        let response = reqwest::Response::from(http::Response::new("sensitive non-JSON response"));
        let source = response.json::<Value>().await.unwrap_err();
        let error = upstream_error("解析公钥响应", source).to_string();
        assert!(error.contains("IAAA 解析公钥响应失败"));
        assert!(error.contains("无法解析 JSON"));
        assert!(!error.contains("sensitive"));
    }

    #[test]
    fn upstream_status_error_reports_http_status_without_secret_url() {
        let response = reqwest::Response::from(http::Response::builder().status(503).body("").unwrap());
        let source = response.error_for_status().unwrap_err();
        let error = upstream_error("获取公钥（getPublicKey.do）", source).to_string();
        assert!(error.contains("getPublicKey.do"));
        assert!(error.contains("HTTP 503"));
    }

    #[test]
    fn credential_rejection_is_distinct_from_verification_challenges() {
        let credential = verify_response(&json!({"success":false,"errors":{"code":"E01"}})).unwrap_err().to_string();
        assert!(credential.contains("E01"));
        assert!(credential.contains("统一身份认证密码"));
        assert!(!credential.contains("图形验证码"));
        let challenge = verify_response(&json!({"success":false,"showCode":true,"errors":{"code":"E01"}})).unwrap_err().to_string();
        assert!(challenge.contains("同时要求图形验证码"));
        let captcha = verify_response(&json!({"success":false,"errors":{"code":"E03"}})).unwrap_err().to_string();
        assert!(captcha.contains("图形验证码"));
        assert!(!captcha.contains("密码"));
        let unknown = verify_response(&json!({"success":false,"errors":{"code":"secret\ncode","msg":"secret-response"}})).unwrap_err().to_string();
        assert!(!unknown.contains("secret"));
    }
}
