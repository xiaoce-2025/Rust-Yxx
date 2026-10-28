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
        .build()?;
    client.post(format!("{IAAA}oauth.jsp"))
        .form(&[("appID", "blackboard"), ("appName", "1"), ("redirectUrl", REDIRECT)])
        .send().await?.error_for_status()?;
    let key: Value = client.get(format!("{IAAA}getPublicKey.do"))
        .send().await?.error_for_status()?.json().await?;
    let key = key.get("key").and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| ApiError::Internal("无法获取 IAAA 公钥，请稍后重试".into()))?;
    let encrypted = super::app_replay::encrypt_binding_password(key, password)
        .map_err(|_| ApiError::Internal("IAAA 密码加密失败，请稍后重试".into()))?;
    let result: Value = client.post(format!("{IAAA}oauthlogin.do"))
        .header("Referer", format!("{IAAA}oauth.jsp"))
        .header("Origin", "https://iaaa.pku.edu.cn")
        .header("X-Requested-With", "XMLHttpRequest")
        .form(&[("appid", "blackboard"), ("userName", username), ("password", encrypted.as_str()),
            ("randCode", ""), ("smsCode", ""), ("otpCode", ""),
            ("remTrustChk", "false"), ("redirUrl", REDIRECT)])
        .send().await?.error_for_status()?.json().await?;
    verify_response(&result)
}

fn verify_response(result: &Value) -> Result<(), ApiError> {
    if result.get("success").and_then(Value::as_bool) == Some(true)
        && result.get("token").and_then(Value::as_str).is_some_and(|t| !t.trim().is_empty()) {
        // The one-use upstream token is deliberately discarded, never returned or stored.
        return Ok(());
    }
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
}
