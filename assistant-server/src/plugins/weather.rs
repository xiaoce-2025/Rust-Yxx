use async_trait::async_trait;
use serde::Deserialize;

use crate::plugins::base::MessageModule;

pub struct WeatherModule;

#[derive(Deserialize)]
struct WeatherApiPayload {
    #[serde(rename = "cityInfo")]
    city_info: Option<CityInfo>,
    data: Option<WeatherData>,
}

#[derive(Deserialize)]
struct CityInfo {
    city: Option<String>,
}

#[derive(Deserialize)]
struct WeatherData {
    forecast: Option<Vec<Forecast>>,
}

#[derive(Deserialize)]
struct Forecast {
    ymd: Option<String>,
    #[serde(rename = "type")]
    weather_type: Option<String>,
    low: Option<String>,
    high: Option<String>,
    fx: Option<String>,
    fl: Option<String>,
}

#[async_trait]
impl MessageModule for WeatherModule {
    async fn handle(&self, _text: &str) -> String {
        let resp = match reqwest::get("http://t.weather.itboy.net/api/weather/city/101010200").await {
            Ok(v) => v,
            Err(e) => return format!("天气查询失败: {e}"),
        };

        let payload = match resp.json::<WeatherApiPayload>().await {
            Ok(v) => v,
            Err(e) => return format!("天气查询失败: {e}"),
        };

        let city = payload
            .city_info
            .and_then(|c| c.city)
            .unwrap_or_else(|| "未知地区".to_string());

        let Some(today) = payload
            .data
            .and_then(|d| d.forecast)
            .and_then(|mut f| if f.is_empty() { None } else { Some(f.remove(0)) })
        else {
            return "天气查询失败: forecast 为空".to_string();
        };

        format!(
            "{} {} 天气: {}，{}，{}，风向风力: {} {}",
            city,
            today.ymd.unwrap_or_else(|| "今天".to_string()),
            today.weather_type.unwrap_or_else(|| "未知".to_string()),
            today.low.unwrap_or_default(),
            today.high.unwrap_or_default(),
            today.fx.unwrap_or_else(|| "未知风向".to_string()),
            today.fl.unwrap_or_else(|| "未知风力".to_string())
        )
    }
}
