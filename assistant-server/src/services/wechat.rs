use base64::{engine::general_purpose::STANDARD, Engine};
use rand::Rng;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{config::WeChatConfig, error::ApiError, models::wechat::{TokenResponse, WeChatUserInfo, WeixinGenericResponse, WeixinUpdatesResponse}};

#[derive(Debug, Clone)]
pub struct WeChatService {
    pub oauth_app_id: String,
    pub oauth_app_secret: String,
    pub redirect_uri: String,
    pub api_base_url: String,
    pub qr_bot_type: String,
    pub http_client: reqwest::Client,
}

impl WeChatService {
    pub fn new(cfg: WeChatConfig) -> Self {
        Self {
            oauth_app_id: cfg.oauth_app_id,
            oauth_app_secret: cfg.oauth_app_secret,
            redirect_uri: cfg.redirect_uri,
            api_base_url: cfg.api_base_url.trim_end_matches('/').to_string(),
            qr_bot_type: cfg.qr_bot_type,
            http_client: reqwest::Client::new(),
        }
    }

    pub fn generate_auth_url(&self, state: &str) -> String {
        format!(
            "https://open.weixin.qq.com/connect/qrconnect?appid={}&redirect_uri={}&response_type=code&scope=snsapi_login&state={}#wechat_redirect",
            self.oauth_app_id,
            urlencoding::encode(&self.redirect_uri),
            state,
        )
    }

    pub async fn get_access_token(&self, code: &str) -> Result<(String, String), ApiError> {
        let url = format!(
            "https://api.weixin.qq.com/sns/oauth2/access_token?appid={}&secret={}&code={}&grant_type=authorization_code",
            self.oauth_app_id, self.oauth_app_secret, code,
        );

        let token_info = self.http_client.get(url).send().await?.json::<TokenResponse>().await?;

        if token_info.access_token.is_empty() || token_info.openid.is_empty() {
            return Err(ApiError::BadRequest(format!(
                "wechat token exchange failed: {:?} {:?}",
                token_info.errcode, token_info.errmsg
            )));
        }

        Ok((token_info.access_token, token_info.openid))
    }

    pub async fn get_user_info(&self, access_token: &str, openid: &str) -> Result<WeChatUserInfo, ApiError> {
        let url = format!(
            "https://api.weixin.qq.com/sns/userinfo?access_token={}&openid={}&lang=zh_CN",
            access_token, openid
        );
        let user_info = self.http_client.get(url).send().await?.json::<WeChatUserInfo>().await?;
        Ok(user_info)
    }

    pub async fn bind_wechat_to_user(&self, _user_id: Uuid, _wechat_info: &WeChatUserInfo) -> Result<(), ApiError> {
        // File-based storage: binding operations handled by FileStore
        Ok(())
    }

    fn random_wechat_uin() -> String {
        let value: u32 = rand::thread_rng().gen_range(1..u32::MAX);
        STANDARD.encode(value.to_be_bytes())
    }

    fn ilink_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert("AuthorizationType", HeaderValue::from_static("ilink_bot_token"));
        headers.insert("X-WECHAT-UIN", HeaderValue::from_str(&Self::random_wechat_uin()).unwrap_or(HeaderValue::from_static("AAAAAA==")));
        if !token.is_empty() {
            let bearer = format!("Bearer {token}");
            if let Ok(v) = HeaderValue::from_str(&bearer) {
                headers.insert("Authorization", v);
            }
        }
        headers
    }

    pub async fn get_bot_qrcode(&self, bot_type: &str) -> Result<Value, ApiError> {
        let url = format!("{}/ilink/bot/get_bot_qrcode", self.api_base_url);
        let body = self
            .http_client
            .get(url)
            .query(&[("bot_type", bot_type)])
            .send()
            .await?
            .json::<Value>()
            .await?;
        Ok(body)
    }

    pub async fn get_qrcode_status(&self, qrcode: &str) -> Result<Value, ApiError> {
        let url = format!("{}/ilink/bot/get_qrcode_status", self.api_base_url);
        let body = self
            .http_client
            .get(url)
            .query(&[("qrcode", qrcode)])
            .header("iLink-App-ClientVersion", "1")
            .send()
            .await?
            .json::<Value>()
            .await?;
        Ok(body)
    }

    pub async fn get_updates(&self, token: &str, cursor: &str) -> Result<WeixinUpdatesResponse, ApiError> {
        let url = format!("{}/ilink/bot/getupdates", self.api_base_url);
        let payload = json!({
            "get_updates_buf": cursor,
            "base_info": { "channel_version": "rust-assistant-server/0.1.0" }
        });
        let resp = self
            .http_client
            .post(url)
            .headers(Self::ilink_headers(token))
            .json(&payload)
            .send()
            .await?;

        let data = resp.json::<WeixinUpdatesResponse>().await?;
        Ok(data)
    }

    pub async fn send_text_message(
        &self,
        token: &str,
        to_user_id: &str,
        text: &str,
        context_token: &str,
    ) -> Result<WeixinGenericResponse, ApiError> {
        let url = format!("{}/ilink/bot/sendmessage", self.api_base_url);
        let payload = json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": to_user_id,
                "client_id": format!("rust-weixin-{}", Uuid::new_v4().simple()),
                "message_type": 2,
                "message_state": 2,
                "context_token": context_token,
                "item_list": [{"type": 1, "text_item": {"text": text}}]
            },
            "base_info": { "channel_version": "rust-assistant-server/0.1.0" }
        });

        let data = self
            .http_client
            .post(url)
            .headers(Self::ilink_headers(token))
            .json(&payload)
            .send()
            .await?
            .json::<WeixinGenericResponse>()
            .await?;

        Ok(data)
    }

    pub fn normalize_weixin_user_id(user_id: &str) -> String {
        let clean = user_id.trim();
        if clean.is_empty() {
            return String::new();
        }
        if clean.contains('@') {
            return clean.to_string();
        }
        format!("{clean}@im.wechat")
    }

    pub fn extract_text(message: &crate::models::wechat::WeixinMessage) -> String {
        let mut texts: Vec<String> = Vec::new();
        for item in &message.item_list {
            if item.item_type == 1 {
                if let Some(text_item) = &item.text_item {
                    if let Some(text) = &text_item.text {
                        if !text.trim().is_empty() {
                            texts.push(text.clone());
                        }
                    }
                }
            }
        }
        texts.join("\n").trim().to_string()
    }
}
