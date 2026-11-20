use super::ui::site_page;
use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    services::recharge::{valid_payment_id, Plan, MAX_CUSTOM_CREDITS},
    store::StoreError,
};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{header::CACHE_CONTROL, HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/recharge/orders", post(create_order))
        .route("/api/recharge/orders/:payment_id", get(order_status))
        .route("/api/recharge/webhook", post(webhook))
        .layer(DefaultBodyLimit::max(8 * 1024))
}

pub(super) async fn page(State(state): State<AppState>) -> Html<String> {
    let plus = Plan::Plus.price(None).expect("fixed price");
    let pro = Plan::Pro.price(None).expect("fixed price");
    let max = Plan::Max.price(None).expect("fixed price");
    let custom = Plan::Custom.price(Some(1)).expect("custom unit price");
    let html = include_str!("../../templates/recharge.html")
        .replace("{{plus}}", &plus.credits.to_string())
        .replace("{{pro}}", &pro.credits.to_string())
        .replace("{{max}}", &max.credits.to_string())
        .replace("{{custom_yuan}}", &(custom.cents / 100).to_string())
        .replace("{{custom_max}}", &MAX_CUSTOM_CREDITS.to_string())
        .replace(
            "{{checkout_disabled}}",
            if state.recharge.is_some() {
                ""
            } else {
                "disabled"
            },
        )
        .replace(
            "{{unavailable_hidden}}",
            if state.recharge.is_some() {
                "hidden"
            } else {
                ""
            },
        );
    site_page(&html, "/user-center")
}

#[derive(Deserialize)]
struct RechargeRequest {
    request_id: Uuid,
    plan: Plan,
    credits: Option<i64>,
}

fn store_error(error: StoreError) -> ApiError {
    tracing::error!(%error, "希元充值数据库操作失败");
    ApiError::Internal("暂时无法处理充值，请稍后重试".into())
}

fn gateway_error() -> ApiError {
    ApiError::Internal("支付中心暂时无法创建此订单，请稍后重试".into())
}

async fn create_order(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(input): Json<RechargeRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let gateway = state.recharge.as_ref().ok_or_else(gateway_error)?;
    let price = input
        .plan
        .price(input.credits)
        .ok_or_else(|| ApiError::BadRequest("请选择有效的充值数量".into()))?;
    let id = input.request_id;
    let local = state
        .store
        .reserve_recharge(id, user.id, &gateway.app_id, gateway.environment, &price)
        .await
        .map_err(store_error)?
        .ok_or_else(|| ApiError::Conflict("该充值请求已用于另一笔订单".into()))?;
    let order = gateway.create_order(id, &price).await.map_err(|error| {
        tracing::warn!(%id, status=?error.status(), timeout=error.is_timeout(), "支付中心下单失败");
        gateway_error()
    })?;
    if !order.valid()
        || order.app_order_id != id
        || order.product_id != price.product
        || order.quantity != price.quantity
        || order.amount != price.cents
        || order.environment != Some(gateway.environment)
        || local
            .payment_id
            .as_ref()
            .is_some_and(|known| known != &order.id)
    {
        tracing::error!(%id, "支付中心订单与本地报价不一致");
        return Err(gateway_error());
    }
    let url = match order.status.as_str() {
        "paid" => format!("/recharge?payment_id={}", order.id),
        "pending" | "confirming" => gateway
            .checkout_url(&order)
            .ok_or_else(gateway_error)?
            .to_string(),
        "closed" | "expired" => {
            return Err(ApiError::Conflict("订单已失效，请重新创建订单".into()))
        }
        _ => return Err(gateway_error()),
    };
    if !state
        .store
        .attach_recharge_payment(id, &order.id)
        .await
        .map_err(store_error)?
    {
        return Err(gateway_error());
    }
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(json!({"checkout_url": url})),
    ))
}

async fn order_status(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(payment_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    if !valid_payment_id(&payment_id) {
        return Err(ApiError::NotFound);
    }
    let order = state
        .store
        .recharge_by_payment(user.id, &payment_id)
        .await
        .map_err(store_error)?
        .ok_or(ApiError::NotFound)?;
    let balance = state
        .store
        .note_balance(user.id)
        .await
        .map_err(store_error)?;
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(json!({
            "credited": order.wechat_order_id.is_some(), "credits": order.credits, "balance": balance,
        })),
    ))
}

async fn webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let gateway = state.recharge.as_ref().ok_or_else(gateway_error)?;
    let event = gateway
        .verify_event(&headers, &body, chrono::Utc::now().timestamp())
        .ok_or(ApiError::Unauthorized)?;
    if !state
        .store
        .settle_recharge(&event.app_id, &event.order)
        .await
        .map_err(store_error)?
    {
        tracing::warn!(payment_id=%event.order.id, "支付回调与本地充值订单不一致");
        return Err(ApiError::BadRequest("支付订单不匹配".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Settings,
        store::{FileStore, FileUser, FileUserSession},
    };
    use chrono::{Duration, Utc};
    use hmac::{Hmac, Mac};
    use serde_json::Value;
    use sha2::Sha256;
    use std::sync::Arc;
    use tokio::net::TcpListener;

    const PAYMENT_ID: &str = "0123456789abcdef0123456789abcdef";
    const CHECKOUT_TOKEN: &str = "123456789abcdef0123456789abcdef0";
    const API_KEY: &str = "local-test-payment-api-key-0000000";
    const HOOK_KEY: &str = "local-test-webhook-hmac-key-000000";

    async fn create_user(store: &FileStore, balance: i64) -> (Uuid, String) {
        let now = Utc::now();
        let id = Uuid::new_v4();
        let token = Uuid::new_v4();
        store
            .insert_user(FileUser {
                id,
                username: id.to_string(),
                email: format!("{id}@example.test"),
                password_hash: "unused".into(),
                is_active: true,
                created_at: now,
                updated_at: now,
                storage_quota: 3,
                processing_quota: balance,
                processing_priority: 5,
                iaaa_encrypted_password: None,
                iaaa_public_key: None,
                iaaa_key_version: None,
                iaaa_username: None,
            })
            .await
            .unwrap();
        store
            .insert_session(FileUserSession {
                session_id: token,
                user_id: id,
                expires_at: now + Duration::hours(1),
                user_agent: None,
                ip_address: None,
                created_at: now,
            })
            .await
            .unwrap();
        (id, format!("assistant_token={token}"))
    }

    #[test]
    fn four_recharge_prices_are_computed_in_cents() {
        for (plan, credits, points, cents, quantity) in [
            (Plan::Plus, None, 30, 3000, 1),
            (Plan::Pro, None, 60, 6000, 1),
            (Plan::Max, None, 120, 12000, 1),
            (Plan::Custom, Some(7), 7, 1400, 7),
        ] {
            let price = plan.price(credits).unwrap();
            assert_eq!(
                (price.credits, price.cents, price.quantity),
                (points, cents, quantity)
            );
        }
        assert!(Plan::Custom.price(Some(0)).is_none());
        assert!(Plan::Custom.price(Some(MAX_CUSTOM_CREDITS + 1)).is_none());
        assert!(Plan::Plus.price(Some(1)).is_none());
    }

    #[tokio::test]
    async fn recharge_checks_gateway_quote_and_credits_signed_order_once() {
        let temp = tempdir::TempDir::new("recharge").unwrap();
        let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_url = format!("http://{}", gateway_listener.local_addr().unwrap());
        let gateway = Router::new().route("/api/v1/orders", post(
            |State(url): State<String>, headers: HeaderMap, Json(input): Json<Value>| async move {
                assert_eq!(headers["authorization"], format!("Bearer {API_KEY}"));
                assert_eq!(headers["x-yxx-pay-app"], "autonote");
                assert_eq!(input["productId"], "credits_custom");
                assert_eq!(input["quantity"], 7);
                assert_eq!(input["expectedAmount"], 1400);
                assert_eq!(input["expectedEnvironment"], 0);
                Json(json!({
                    "id": PAYMENT_ID, "appOrderId": input["appOrderId"], "productId": "credits_custom",
                    "quantity": 7, "amount": 1400, "currency": "CNY", "status": "pending",
                    "method": "wechat_virtual", "environment": 0,
                    "checkoutUrl": format!("{url}/checkout/{CHECKOUT_TOKEN}"),
                }))
            },
        )).with_state(gateway_url.clone());
        let gateway_task = tokio::spawn(async move {
            axum::serve(gateway_listener, gateway).await.unwrap();
        });

        let settings: Settings = serde_json::from_value(json!({
            "server":{"address":"127.0.0.1","port":0},
            "wechat":{"api_base_url":"","oauth_app_id":"","oauth_app_secret":"","redirect_uri":"","qr_bot_type":""},
            "security":{"session_ttl_hours":1},"cors":{"allowed_origins":[],"allow_credentials":false},
            "audioapi_paraformer":{},"llm_deepseek":{},
            "recharge":{"enabled":true,"base_url":gateway_url,"api_key":API_KEY,"webhook_secret":HOOK_KEY},
        })).unwrap();
        let store = Arc::new(FileStore::new(temp.path().into()).await.unwrap());
        let (owner, cookie) = create_user(&store, -2).await;
        let (_, other_cookie) = create_user(&store, 0).await;
        let state = AppState::new(settings, store.clone());
        let app = super::super::router(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app_task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let page = client.get(format!("{base}/recharge")).send().await.unwrap();
        assert_eq!(page.status(), 303);
        let page = client
            .get(format!("{base}/recharge"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(page.contains("自定义充值") && page.contains("¥120"));
        assert!(!page.contains(API_KEY) && !page.contains(HOOK_KEY));
        let center = client
            .get(format!("{base}/user-center"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(center.contains("href=\"/recharge\">充值希元"));

        let id = Uuid::new_v4();
        let create = |cookie: &str| {
            client
                .post(format!("{base}/api/recharge/orders"))
                .header("cookie", cookie)
                .json(&json!({"request_id":id,"plan":"custom","credits":7}))
        };
        assert_eq!(create("").send().await.unwrap().status(), 401);
        let order: Value = create(&cookie)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            order["checkout_url"],
            format!("{gateway_url}/checkout/{CHECKOUT_TOKEN}")
        );
        assert_eq!(create(&other_cookie).send().await.unwrap().status(), 409);
        let status = |cookie: &str| {
            client
                .get(format!("{base}/api/recharge/orders/{PAYMENT_ID}"))
                .header("cookie", cookie)
        };
        assert_eq!(status(&other_cookie).send().await.unwrap().status(), 404);
        let before: Value = status(&cookie).send().await.unwrap().json().await.unwrap();
        assert_eq!(before, json!({"credited":false,"credits":7,"balance":-2}));

        let event = format!("payment.paid.{PAYMENT_ID}");
        let hook = |key: &str, amount, environment, age| {
            let body = json!({"eventId":event,"type":"payment.paid","appId":"autonote","order":{
                "id":PAYMENT_ID,"appOrderId":id,"productId":"credits_custom","quantity":7,
                "amount":amount,"environment":environment,"method":"wechat_virtual","currency":"CNY",
                "status":"paid","wechatOrderId":"wx-order-test"
            }}).to_string();
            let timestamp = Utc::now().timestamp() - age;
            let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
            mac.update(format!("{timestamp}.{body}").as_bytes());
            client
                .post(format!("{base}/api/recharge/webhook"))
                .header("x-yxx-pay-event", &event)
                .header("x-yxx-pay-timestamp", timestamp)
                .header(
                    "x-yxx-pay-signature",
                    format!("sha256={:x}", mac.finalize().into_bytes()),
                )
                .body(body)
                .send()
        };
        assert_eq!(hook("wrong key", 1400, 0, 0).await.unwrap().status(), 401);
        assert_eq!(hook(HOOK_KEY, 1400, 0, 301).await.unwrap().status(), 401);
        assert_eq!(hook(HOOK_KEY, 200, 0, 0).await.unwrap().status(), 400);
        assert_eq!(hook(HOOK_KEY, 1400, 1, 0).await.unwrap().status(), 400);
        assert_eq!(store.note_balance(owner).await.unwrap(), -2);
        let (first, second) = tokio::join!(hook(HOOK_KEY, 1400, 0, 0), hook(HOOK_KEY, 1400, 0, 0));
        assert_eq!(first.unwrap().status(), 204);
        assert_eq!(second.unwrap().status(), 204);
        let after: Value = status(&cookie).send().await.unwrap().json().await.unwrap();
        assert_eq!(after, json!({"credited":true,"credits":7,"balance":5}));
        let ledger = store.note_ledger(owner, 30, None).await.unwrap();
        let entries = ledger["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["event_type"], "RECHARGE");

        app_task.abort();
        gateway_task.abort();
    }
}
