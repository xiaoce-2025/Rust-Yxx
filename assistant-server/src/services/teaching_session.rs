//! Teaching access without persisting university credentials.
use super::pkulogin::{self, PkuLoginResult};
use crate::{app_state::AppState, store::FileUser};
use uuid::Uuid;

pub const LOGIN_REQUIRED: &str = "尚未绑定教学网或一次性登录已失效，请选择绑定账号或一次性登录";

pub fn cache_key(user: Uuid) -> String {
    format!("teaching-user:{user}")
}

pub async fn cached(state: &AppState, user: &FileUser) -> Option<PkuLoginResult> {
    if let Some(session) = state.login_cache.get(&cache_key(user.id)).await {
        return Some(session);
    }
    // Legacy, bound-account sessions remain compatible; unbound users cannot borrow them.
    if user.iaaa_encrypted_password.is_some() {
        return state
            .login_cache
            .get(user.email.split('@').next().unwrap_or_default())
            .await;
    }
    None
}

pub async fn login(state: &AppState, user: &FileUser) -> Result<PkuLoginResult, String> {
    if let Some(session) = cached(state, user).await {
        return Ok(session);
    }
    let password = user
        .iaaa_encrypted_password
        .as_deref()
        .filter(|p| !p.is_empty())
        .ok_or(LOGIN_REQUIRED)?;
    pkulogin::login_iaaa_and_get_cookies_with_cache(
        user.email.split('@').next().unwrap_or_default(),
        password,
        &state.login_cache,
    )
    .await
}
