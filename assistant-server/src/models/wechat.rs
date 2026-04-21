use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeChatUserInfo {
    pub openid: String,
    pub nickname: Option<String>,
    pub headimgurl: Option<String>,
    pub unionid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub openid: String,
    pub errcode: Option<i64>,
    pub errmsg: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeixinMessageItemText {
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeixinMessageItem {
    #[serde(rename = "type")]
    pub item_type: i32,
    pub text_item: Option<WeixinMessageItemText>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeixinMessage {
    pub message_id: Option<i64>,
    pub from_user_id: Option<String>,
    pub to_user_id: Option<String>,
    pub create_time_ms: Option<i64>,
    pub message_type: Option<i32>,
    pub message_state: Option<i32>,
    pub context_token: Option<String>,
    #[serde(default)]
    pub item_list: Vec<WeixinMessageItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeixinUpdatesResponse {
    pub ret: i32,
    pub errcode: Option<i32>,
    pub errmsg: Option<String>,
    #[serde(default)]
    pub msgs: Vec<WeixinMessage>,
    #[serde(default)]
    pub get_updates_buf: String,
    pub longpolling_timeout_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeixinGenericResponse {
    pub ret: i32,
    pub errcode: Option<i32>,
    pub errmsg: Option<String>,
}
