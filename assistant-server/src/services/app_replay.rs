pub use super::app_replay_client::AppReplayClient;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::json;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualReplay {
    pub course_id: String,
    pub sub_id: String,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub lesson_start: Option<u8>,
    #[serde(default)]
    pub lesson_end: Option<u8>,
}

pub fn encrypt_binding_password(key: &str, password: &str) -> Result<String, String> {
    super::app_replay_auth::encrypt_password(key, password)
}

pub fn save_binding(user: uuid::Uuid, account: &str, password: &str) -> Result<(), String> {
    super::app_replay_vault::save(
        user,
        "binding",
        &serde_json::json!({"account":account,"password":password}),
    )
}
pub fn clear_binding(user: uuid::Uuid) -> Result<(), String> {
    super::app_replay_vault::remove(user, "binding")?;
    super::app_replay_vault::remove(user, "session")
}
pub fn clear_session(user: uuid::Uuid) -> Result<(), String> {
    super::app_replay_vault::remove(user, "session")
}

pub fn validate_periods(start: u8, end: u8) -> Result<(), String> {
    if !(1..=12).contains(&start) || end < start || end > 12 {
        return Err("课节范围必须为1–12节，结束节不能早于开始节".into());
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct SemesterConfig {
    pub terms: std::collections::HashMap<String, Semester>,
    #[serde(default)]
    pub app_course_ids: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Semester {
    pub start: chrono::NaiveDate,
    pub end: chrono::NaiveDate,
}

impl SemesterConfig {
    pub fn load() -> Result<Self, String> {
        let path = std::env::var("PKU_REPLAY_SEMESTERS")
            .unwrap_or_else(|_| "config/pku_replay_semesters.json".into());
        let text = std::fs::read_to_string(path).map_err(|e| format!("读取学期配置失败: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("解析学期配置失败: {e}"))
    }

    pub fn range(&self, term: &str, today: chrono::NaiveDate) -> Result<Semester, String> {
        let term = canonical_term(term).ok_or_else(|| format!("无法识别教学网学期: {term}"))?;
        let mut range = self
            .terms
            .get(&term)
            .cloned()
            .ok_or_else(|| format!("请在学期配置中添加 {term} 的时间范围"))?;
        if range.start > range.end || (range.end - range.start).num_days() > 370 {
            return Err("学期时间范围无效".into());
        }
        range.end = range.end.min(today);
        if range.start > range.end {
            return Err("该学期尚未开始".into());
        }
        Ok(range)
    }
}

pub fn canonical_term(term: &str) -> Option<String> {
    let re = regex::Regex::new(r"(\d{2,4})\s*[-—–]\s*(\d{2,4})\s*学年\s*第\s*([123一二三])\s*学期")
        .ok()?;
    let parts = re.captures(term)?;
    let year = |s: &str| {
        s.parse::<u32>()
            .ok()
            .map(|n| if n < 100 { n + 2000 } else { n })
    };
    let start = year(&parts[1])?;
    let end = year(&parts[2])?;
    if end != start + 1 {
        return None;
    }
    let number = match &parts[3] {
        "一" => "1",
        "二" => "2",
        "三" => "3",
        n => n,
    };
    Some(format!("{start}-{end}-{number}"))
}

pub fn selected_dates(
    range: &Semester,
    weekdays: &[u32],
    date: Option<chrono::NaiveDate>,
) -> Result<Vec<String>, String> {
    use chrono::Datelike;
    if let Some(date) = date {
        if !weekdays.is_empty() {
            return Err("请选择按星期或指定日期其中一种方式".into());
        }
        if date < range.start || date > range.end {
            return Err("日期必须在该学期内且不能晚于今天".into());
        }
        return Ok(vec![date.to_string()]);
    }
    if weekdays.is_empty() || weekdays.iter().any(|d| !(1..=7).contains(d)) {
        return Err("请至少选择一个星期（1至7）".into());
    }
    Ok(range
        .start
        .iter_days()
        .take_while(|d| *d <= range.end)
        .filter(|d| weekdays.contains(&d.weekday().number_from_monday()))
        .map(|d| d.to_string())
        .collect())
}

pub fn today() -> chrono::NaiveDate {
    (chrono::Utc::now() + chrono::Duration::hours(8)).date_naive()
}

#[cfg(test)]
mod tests {
    #[test]
    fn periods_are_limited_to_twelve_and_ordered() {
        assert!(super::validate_periods(1, 12).is_ok());
        assert!(super::validate_periods(6, 6).is_ok());
        for (start, end) in [(0, 2), (1, 13), (8, 7), (13, 13)] {
            assert!(super::validate_periods(start, end).is_err());
        }
    }
    use super::*;
    #[test]
    fn term_and_dates_are_bounded() {
        assert_eq!(
            canonical_term("26-27学年第1学期").as_deref(),
            Some("2026-2027-1")
        );
        assert_eq!(
            canonical_term("2026—2027学年第二学期").as_deref(),
            Some("2026-2027-2")
        );
        assert!(canonical_term("2026-2028学年第1学期").is_none());
        let config: SemesterConfig = serde_json::from_value(json!({"terms":{
            "2026-2027-1":{"start":"2026-09-07","end":"2027-01-10"}}}))
        .unwrap();
        let range = config
            .range("26-27学年第1学期", "2026-09-14".parse().unwrap())
            .unwrap();
        assert_eq!(
            selected_dates(&range, &[1, 1, 3], None).unwrap(),
            ["2026-09-07", "2026-09-09", "2026-09-14"]
        );
        assert!(selected_dates(&range, &[], Some("2026-09-15".parse().unwrap())).is_err());
        assert!(selected_dates(&range, &[8], None).is_err());
    }
}
