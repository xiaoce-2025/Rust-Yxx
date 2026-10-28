//! Fresh IAAA verification for signing in to the assistant. Never follows the teaching callback.
use reqwest::{redirect::Policy, Client};
use serde_json::Value;
use std::time::Duration;

use crate::error::ApiError;

const IAAA: &str = "https://iaaa.pku.edu.cn/iaaa/";
const REDIRECT: &str = "http://course.pku.edu.cn/webapps/bb-sso-BBLEARN/execute/authValidate/campusLogin";

pub async fn verify(username: &str, password: &str) -> Result<(), ApiError> {
    // An isolated cookie jar and a fresh request are essential: cached teaching sessions
    // must never be accepted as proof that the submitted password is correct.
    let client = Client::builder()
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
    let key = key.get("key").and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| ApiError::Internal("无法获取 IAAA 公钥，请稍后重试".into()))?;
    let encrypted = super::app_replay::encrypt_binding_password(key, password)
        .map_err(|_| ApiError::Internal("IAAA 密码加密失败，请稍后重试".into()))?;
    tracing::debug!(host="iaaa.pku.edu.cn", stage="校验凭据", "IAAA public key loaded; submitting encrypted credentials");
    let result: Value = client.post(format!("{IAAA}oauthlogin.do"))
        .header("Referer", format!("{IAAA}oauth.jsp"))
        .header("Origin", "https://iaaa.pku.edu.cn")
        .header("X-Requested-With", "XMLHttpRequest")
        .form(&[("appid", "blackboard"), ("userName", username), ("password", encrypted.as_str()),
            ("randCode", ""), ("smsCode", ""), ("otpCode", ""),
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
    let code = result.pointer("/errors/code").and_then(Value::as_str)
        .filter(|code| code.len() <= 64 && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
    let success = result.get("success").and_then(Value::as_bool);
    let token_present = result.get("token").and_then(Value::as_str).is_some_and(|t| !t.trim().is_empty());
    tracing::warn!(host="iaaa.pku.edu.cn", stage="校验登录响应", iaaa_code=code,
        success=?success, token_present,
        "IAAA rejected authentication or returned no usable token");
    Err(ApiError::BadRequest("IAAA 认证未通过，请检查学号和密码；如需验证码或二次验证，请先前往 IAAA 完成验证后重试".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
