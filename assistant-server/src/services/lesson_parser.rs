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
            enc_key: None,
            id: format!("{}_lesson_{}", course_id, index),
            video_url: String::new(),
            play_url,
            date,
            time,
            day_of_week,
            is_processed: false,
            tex_filename: None,
            manual_replay: None,
        };

        lessons.push(lesson);
    }

    tracing::info!("解析出 {} 个课节", lessons.len());
    lessons
}

pub fn has_next_page(html: &str) -> bool {
    get_next_page_url(html).is_some()
}

pub fn get_next_page_url(html: &str) -> Option<String> {
    let document = Html::parse_document(html);
    let next_page_selector = Selector::parse("a#listContainer_nextpage_bot").unwrap();
    
    if let Some(next_page_link) = document.select(&next_page_selector).next() {
        if let Some(class_name) = next_page_link.value().attr("class") {
            if class_name.split_whitespace().any(|c| c.eq_ignore_ascii_case("disabled")) {
                return None;
            }
        }

        if let Some(aria_disabled) = next_page_link.value().attr("aria-disabled") {
            if aria_disabled.eq_ignore_ascii_case("true") {
                return None;
            }
        }

        if let Some(href) = next_page_link.value().attr("href") {
            let href = href.trim();

            if href.is_empty() || href == "#" || href.to_ascii_lowercase().starts_with("javascript:") {
                return None;
            }

            if href.starts_with("http://") || href.starts_with("https://") {
                if is_video_list_url(href) {
                    return Some(href.to_string());
                }
                return None;
            }

            if href.starts_with('/') {
                let full_url = format!("https://course.pku.edu.cn{}", href);
                if is_video_list_url(&full_url) {
                    return Some(full_url);
                }
                return None;
            }

            let full_url = format!("https://course.pku.edu.cn/{}", href.trim_start_matches('/'));
            if is_video_list_url(&full_url) {
                return Some(full_url);
            }
            return None;
        }
    }
    
    None
}

fn is_video_list_url(url: &str) -> bool {
    // 只接受视频课节列表分页URL，避免误跟随到课程导航/工具页造成循环。
    url.contains("/webapps/bb-streammedia-hqy-BBLEARN/videoList.action")
}

pub fn assign_lesson_numbers(lessons: &mut Vec<LessonInfo>) {
    sort_lessons(lessons);
    for (index, lesson) in lessons.iter_mut().enumerate() {
        lesson.id = format!("lesson_{}", index + 1);
    }
}

pub fn sort_lessons(lessons: &mut [LessonInfo]) {
    lessons.sort_by(|a, b| {
        let date_a = NaiveDate::parse_from_str(&a.date, "%Y-%m-%d").unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());
        let date_b = NaiveDate::parse_from_str(&b.date, "%Y-%m-%d").unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());
        
        let time_order_a = parse_time_order(&a.time);
        let time_order_b = parse_time_order(&b.time);
        
        match date_a.cmp(&date_b) {
            std::cmp::Ordering::Equal => time_order_a.cmp(&time_order_b),
            other => other,
        }
    });
    
}

fn parse_time_order(time_str: &str) -> i32 {
    let re = regex::Regex::new(r"第(\d+)").unwrap();
    if let Some(captures) = re.captures(time_str) {
        if let Ok(num) = captures[1].parse::<i32>() {
            let starts = [480, 540, 610, 670, 780, 840, 910, 970, 1030, 1120, 1180, 1240];
            return starts.get(num.saturating_sub(1) as usize).copied().unwrap_or(num * 60);
        }
    }
    let clock = regex::Regex::new(r"(\d{1,2}):(\d{2})").unwrap();
    if let Some(c) = clock.captures(time_str) {
        if let (Ok(hours), Ok(minutes)) = (c[1].parse::<i32>(), c[2].parse::<i32>()) {
            return hours * 60 + minutes;
        }
    }
    0
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
        assert_eq!(calculate_day_of_week("2026-04-30"), "周四");
        assert_eq!(calculate_day_of_week("2026-05-01"), "周五");
        assert_eq!(calculate_day_of_week("2026-05-03"), "周日");
    }

    #[test]
    fn test_assign_lesson_numbers() {
        let mut lessons = vec![
            LessonInfo {
                enc_key: None,
                id: "test_1".to_string(),
                video_url: "".to_string(),
                play_url: "".to_string(),
                date: "2026-05-02".to_string(),
                time: "第3-4节".to_string(),
                day_of_week: "周五".to_string(),
                is_processed: false,
                tex_filename: None,
                manual_replay: None,
            },
            LessonInfo {
                enc_key: None,
                id: "test_2".to_string(),
                video_url: "".to_string(),
                play_url: "".to_string(),
                date: "2026-04-30".to_string(),
                time: "第7-9节".to_string(),
                day_of_week: "周三".to_string(),
                is_processed: false,
                tex_filename: None,
                manual_replay: None,
            },
            LessonInfo {
                enc_key: None,
                id: "test_3".to_string(),
                video_url: "".to_string(),
                play_url: "".to_string(),
                date: "2026-05-02".to_string(),
                time: "第1-2节".to_string(),
                day_of_week: "周五".to_string(),
                is_processed: false,
                tex_filename: None,
                manual_replay: None,
            },
        ];
        
        assign_lesson_numbers(&mut lessons);
        
        assert_eq!(lessons[0].id, "lesson_1");
        assert_eq!(lessons[0].date, "2026-04-30");
        
        assert_eq!(lessons[1].id, "lesson_2");
        assert_eq!(lessons[1].date, "2026-05-02");
        assert_eq!(lessons[1].time, "第1-2节");
        
        assert_eq!(lessons[2].id, "lesson_3");
        assert_eq!(lessons[2].date, "2026-05-02");
        assert_eq!(lessons[2].time, "第3-4节");
    }
}
