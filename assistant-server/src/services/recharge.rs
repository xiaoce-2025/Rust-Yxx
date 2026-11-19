use crate::config::RechargeConfig;
use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::Duration;
use uuid::Uuid;

pub const MAX_CUSTOM_CREDITS: i64 = i32::MAX as i64 / 200;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Plan {
    Plus,
    Pro,
    Max,
    Custom,
}

pub struct Price {
    pub product: &'static str,
    pub quantity: i64,
    pub credits: i64,
    pub cents: i64,
}

impl Plan {
    pub fn price(self, custom: Option<i64>) -> Option<Price> {
        let (product, credits) = match self {
            Self::Plus => ("credits_plus", 30),
            Self::Pro => ("credits_pro", 60),
            Self::Max => ("credits_max", 120),
            Self::Custom => {
                let credits = custom.filter(|n| (1..=MAX_CUSTOM_CREDITS).contains(n))?;
                return Some(Price {
                    product: "credits_custom",
                    quantity: credits,
                    credits,
                    cents: credits * 200,
                });
            }
        };
        custom.is_none().then_some(Price {
            product,
            quantity: 1,
            credits,
            cents: credits * 100,
        })
    }
}

pub struct PaymentCenter {
    origin: Url,
    client: Client,
    pub app_id: String,
    pub environment: u8,
    api_key: String,
    webhook_secret: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentOrder {
    pub id: String,
    pub app_order_id: Uuid,
    pub product_id: String,
    #[serde(default = "one")]
    pub quantity: i64,
    pub amount: i64,
    pub currency: String,
    pub status: String,
    pub method: String,
    pub checkout_url: Option<String>,
    pub environment: Option<u8>,
    pub wechat_order_id: Option<String>,
}

fn one() -> i64 {
    1
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentEvent {
    pub event_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub app_id: String,
    pub order: PaymentOrder,
}

pub fn valid_payment_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl PaymentOrder {
    pub fn valid(&self) -> bool {
        valid_payment_id(&self.id) && self.currency == "CNY" && self.method == "wechat_virtual"
    }
}

impl PaymentCenter {
    pub fn from_config(cfg: &RechargeConfig) -> Result<Option<Self>, &'static str> {
        if !cfg.enabled {
            return Ok(None);
        }
        let origin = Url::parse(&cfg.base_url).map_err(|_| "支付中心地址无效")?;
        let loopback = matches!(origin.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if origin.host_str().is_none()
            || !(origin.scheme() == "https" || origin.scheme() == "http" && loopback)
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
            || !origin.username().is_empty()
            || origin.password().is_some()
        {
            return Err("支付中心地址必须为 HTTPS 站点源地址");
        }
        if cfg.app_id.is_empty()
            || cfg.app_id.len() > 64
            || !cfg
                .app_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            || cfg.api_key.trim().len() < 32
            || cfg.webhook_secret.trim().len() < 32
            || cfg.api_key == cfg.webhook_secret
            || cfg.environment > 1
        {
            return Err("支付中心应用编号、支付环境或密钥无效");
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "无法初始化支付客户端")?;
        Ok(Some(Self {
            origin,
            client,
            app_id: cfg.app_id.clone(),
            environment: cfg.environment,
            api_key: cfg.api_key.clone(),
            webhook_secret: cfg.webhook_secret.clone(),
        }))
    }

    pub async fn create_order(
        &self,
        id: Uuid,
        price: &Price,
    ) -> Result<PaymentOrder, reqwest::Error> {
        let input = serde_json::json!({
            "appOrderId": id, "productId": price.product, "quantity": price.quantity,
            "expectedAmount": price.cents, "expectedEnvironment": self.environment,
        });
        self.client
            .post(
                self.origin
                    .join("/api/v1/orders")
                    .expect("valid payment endpoint"),
            )
            .header("X-Yxx-Pay-App", &self.app_id)
            .bearer_auth(&self.api_key)
            .json(&input)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    pub fn checkout_url(&self, order: &PaymentOrder) -> Option<Url> {
        let url = Url::parse(order.checkout_url.as_deref()?).ok()?;
        let token = url.path().strip_prefix("/checkout/")?;
        (url.origin() == self.origin.origin()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && valid_payment_id(token))
        .then_some(url)
    }

    pub fn verify_event(&self, headers: &HeaderMap, body: &[u8], now: i64) -> Option<PaymentEvent> {
        let header = |name| headers.get(name)?.to_str().ok();
        let timestamp = header("x-yxx-pay-timestamp")?;
        if timestamp.parse::<i64>().ok()?.abs_diff(now) > 300 {
            return None;
        }
        let signature = header("x-yxx-pay-signature")?.strip_prefix("sha256=")?;
        let mut bytes = [0; 32];
        hex::decode_to_slice(signature, &mut bytes).ok()?;
        let mut mac = Hmac::<Sha256>::new_from_slice(self.webhook_secret.as_bytes()).ok()?;
        mac.update(timestamp.as_bytes());
        mac.update(b".");
        mac.update(body);
        mac.verify_slice(&bytes).ok()?;

        let event: PaymentEvent = serde_json::from_slice(body).ok()?;
        let order = &event.order;
        (event.app_id == self.app_id
            && event.kind == "payment.paid"
            && event.event_id == header("x-yxx-pay-event")?
            && event.event_id == format!("payment.paid.{}", order.id)
            && order.valid()
            && order.status == "paid"
            && order
                .wechat_order_id
                .as_ref()
                .is_some_and(|id| !id.is_empty() && id.len() <= 128))
        .then_some(event)
    }
}
