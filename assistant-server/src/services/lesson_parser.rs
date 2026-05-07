use scraper::{Html, Selector};
use chrono::{NaiveDate, Datelike};

use super::lesson_storage::LessonInfo;

pub fn parse_lessons(html: &str, course_id: &str) -> Vec<LessonInfo> {
    let document = Html::parse_document(html);
    
    let row_selector = Selector::parse("tr[id^='listContainer_row:']").unwrap();
    let th_selector = Selector::parse("th").unwrap();
    let td_selector = Selector::parse("td").unwrap();
    let a_selector = Selector::parse("a.inlineAction").unwrap();

    let mut lessons = Vec::new();

    for (index, row) in document.select(&row_selector).enumerate() {
        let th_element = row.select(&th_selector).next();
        let tds: Vec<_> = row.select(&td_selector).collect();

        if th_element.is_none() {
            continue;
        }

        let th_text = th_element.unwrap().text().collect::<String>();
        
        let (date_str, time_str) = parse_date_time_from_th(&th_text);
        
        let date = date_str.to_string();
        let time = time_str.to_string();
        let day_of_week = calculate_day_of_week(&date);

        let play_url = if tds.len() >= 3 {
            let action_td = &tds[2];
            if let Some(a_element) = action_td.select(&a_selector).next() {
                if let Some(href) = a_element.value().attr("href") {
                    format!("https://course.pku.edu.cn/webapps/bb-streammedia-hqy-BBLEARN/{}", href)
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let lesson = LessonInfo {
            id: format!("{}_lesson_{}", course_id, index),
            video_url: String::new(),
            play_url,
            date,
            time,
            day_of_week,
            is_processed: false,
            tex_filename: None,
        };

        lessons.push(lesson);
    }

    tracing::info!("解析出 {} 个课节", lessons.len());
    lessons
}

fn parse_date_time_from_th(th_text: &str) -> (&str, &str) {
    let date_re = regex::Regex::new(r"(\d{4}-\d{2}-\d{2})").unwrap();
    let time_re = regex::Regex::new(r"(第\d+[-~]\d+节|第\d+节)").unwrap();

    let date_str = date_re.find(th_text)
        .map(|m| m.as_str())
        .unwrap_or("");
    
    let time_str = time_re.find(th_text)
        .map(|m| m.as_str())
        .unwrap_or("");

    (date_str, time_str)
}

fn calculate_day_of_week(date_str: &str) -> String {
    if let Ok(date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
        let weekday = date.weekday();
        match weekday.num_days_from_monday() {
            0 => "周一".to_string(),
            1 => "周二".to_string(),
            2 => "周三".to_string(),
            3 => "周四".to_string(),
            4 => "周五".to_string(),
            5 => "周六".to_string(),
            6 => "周日".to_string(),
            _ => String::new(),
        }
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_date_time_from_th() {
        let th_text = "2026-04-30第7-9节";
        let (date, time) = parse_date_time_from_th(th_text);
        assert_eq!(date, "2026-04-30");
        assert_eq!(time, "第7-9节");
    }

    #[test]
    fn test_calculate_day_of_week() {
        assert_eq!(calculate_day_of_week("2026-04-30"), "周三");
        assert_eq!(calculate_day_of_week("2026-05-01"), "周四");
        assert_eq!(calculate_day_of_week("2026-05-03"), "周六");
    }
}
