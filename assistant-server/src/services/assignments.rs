use std::{collections::HashSet, sync::LazyLock};

use chrono::{DateTime, FixedOffset, NaiveDate, TimeZone, Utc};
use regex::Regex;
use reqwest::Url;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};

use super::{course_parser, pkulogin, ta::Blackboard};

const ORIGIN: &str = "https://course.pku.edu.cn";
const CONTENT: &str = "/webapps/blackboard/content/listContent.jsp";
const ASSIGNMENT: &str = "/webapps/assignment/uploadAssignment";

#[derive(Deserialize, Serialize)]
pub struct Reminder {
    pub title: String,
    pub course: String,
    pub deadline: DateTime<FixedOffset>,
    pub completed: bool,
}

pub async fn fetch(client: &Blackboard) -> Result<Vec<Reminder>, String> {
    let portal = client
        .get("/webapps/portal/execute/tabs/tabAction?tab_tab_group_id=_1_1")
        .await?;
    let portal = std::str::from_utf8(&portal).map_err(|_| "教学网课程页编码无效")?;
    let courses = current_courses(portal)?;
    let mut reminders = Vec::new();
    for course in courses {
        reminders.extend(course_reminders(client, &course).await?);
    }
    reminders.sort_by(|a, b| {
        a.deadline
            .cmp(&b.deadline)
            .then_with(|| a.course.cmp(&b.course))
            .then_with(|| a.title.cmp(&b.title))
    });
    Ok(reminders)
}

fn current_courses(portal: &str) -> Result<Vec<course_parser::CourseInfo>, String> {
    let mut courses = course_parser::parse_courses(portal);
    if !pkulogin::is_teaching_portal(portal)
        || (courses.is_empty() && portal.contains("type=Course"))
    {
        return Err("无法识别教学网课程列表".into());
    }

    let doc = Html::parse_document(portal);
    let titles = selector(".moduleTitle");
    let links = selector("a[href]");
    let mut current_portlet = false;
    let mut current_keys = HashSet::new();
    for portlet in doc.select(&selector("div.portlet")) {
        let current = portlet.select(&titles).any(|title| {
            let label = title.text().collect::<String>();
            (label.contains("当前") && label.contains("课程"))
                || label.contains("Current Semester Courses")
        });
        if current {
            current_portlet = true;
            current_keys.extend(portlet.select(&links).filter_map(|link| {
                let (_, pk) = link.attr("href")?.split_once("type=Course&id=PkId{key=")?;
                let (key, _) = pk.split_once(',')?;
                Some(key.trim().to_owned())
            }));
        }
    }
    if current_portlet {
        courses.retain(|course| current_keys.contains(&course.key));
    } else if let Some(latest) = courses
        .iter()
        .filter_map(|course| term_order(&course.term))
        .max()
    {
        courses.retain(|course| term_order(&course.term) == Some(latest));
    }
    let mut seen = HashSet::new();
    courses.retain(|course| seen.insert(course.key.clone()));
    Ok(courses)
}

fn term_order(term: &str) -> Option<(u32, u32)> {
    static TERM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(\d{2,4})\s*[-–]\s*\d{2,4}学年\s*第([123])学期").unwrap());
    let parts = TERM.captures(term)?;
    let year: u32 = parts[1].parse().ok()?;
    Some((
        if year < 100 { year + 2000 } else { year },
        parts[2].parse().ok()?,
    ))
}

async fn course_reminders(
    client: &Blackboard,
    course: &course_parser::CourseInfo,
) -> Result<Vec<Reminder>, String> {
    let url = teaching_url(
        "/webapps/blackboard/execute/announcement",
        &[
            ("course_id", &course.key),
            ("method", "search"),
            ("context", "course_entry"),
            ("handle", "announcements_entry"),
            ("mode", "view"),
        ],
    );
    let mut folders = course_folders(&client.get(url.as_str()).await?)?;
    let mut visited = HashSet::new();
    let mut seen_assignments = HashSet::new();
    let mut reminders = Vec::new();
    while let Some(id) = folders.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        if visited.len() > 500 {
            return Err("教学网课程目录超过读取限制".into());
        }
        let url = teaching_url(CONTENT, &[("course_id", &course.key), ("content_id", &id)]);
        let (children, assignments) = folder_entries(&client.get(url.as_str()).await?)?;
        folders.extend(children);
        for (id, title) in assignments {
            if !seen_assignments.insert(id.clone()) {
                continue;
            }
            let url = teaching_url(
                ASSIGNMENT,
                &[
                    ("course_id", &course.key),
                    ("content_id", &id),
                    ("action", "newAttempt"),
                ],
            );
            let Some(deadline) = assignment_deadline(&client.get(url.as_str()).await?)? else {
                continue;
            };
            let url = teaching_url(
                ASSIGNMENT,
                &[
                    ("course_id", &course.key),
                    ("content_id", &id),
                    ("mode", "view"),
                ],
            );
            let completed = assignment_completed(&client.get(url.as_str()).await?)?;
            reminders.push(Reminder {
                title,
                course: html_escape::decode_html_entities(&course.name).into_owned(),
                deadline,
                completed,
            });
        }
    }
    Ok(reminders)
}

fn teaching_url(path: &str, params: &[(&str, &str)]) -> Url {
    Url::parse_with_params(&format!("{ORIGIN}{path}"), params).expect("fixed teaching URL")
}

fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("fixed assignment selector")
}

fn document(bytes: &[u8]) -> Result<Html, String> {
    let html = std::str::from_utf8(bytes).map_err(|_| "教学网页面编码无效")?;
    Ok(Html::parse_document(html))
}

fn content_id(href: &str, path: &str) -> Option<String> {
    let url = Url::parse(ORIGIN).ok()?.join(href).ok()?;
    if url.origin() != Url::parse(ORIGIN).ok()?.origin() || url.path() != path {
        return None;
    }
    url.query_pairs()
        .find(|(key, _)| key == "content_id")
        .map(|(_, value)| value.into_owned())
}

fn course_folders(bytes: &[u8]) -> Result<Vec<String>, String> {
    let page = document(bytes)?;
    let menu = page
        .select(&selector("#courseMenuPalette_contents"))
        .next()
        .ok_or("无法识别教学网课程菜单")?;
    Ok(menu
        .select(&selector("li > a[href]"))
        .filter_map(|link| content_id(link.attr("href")?, CONTENT))
        .collect())
}

type FolderEntries = (Vec<String>, Vec<(String, String)>);

fn folder_entries(bytes: &[u8]) -> Result<FolderEntries, String> {
    let page = document(bytes)?;
    let list = page
        .select(&selector("#content_listContainer"))
        .next()
        .ok_or("无法识别教学网课程目录")?;
    let headings = selector("h3");
    let links = selector("a[href]");
    let mut folders = Vec::new();
    let mut assignments = Vec::new();
    for item in list
        .children()
        .filter_map(scraper::ElementRef::wrap)
        .filter(|element| element.value().name() == "li")
    {
        let is_assignment = item
            .children()
            .filter_map(scraper::ElementRef::wrap)
            .any(|el| matches!(el.attr("alt"), Some("作业" | "Assignment")));
        let Some(heading) = item.select(&headings).next() else {
            continue;
        };
        let link = heading.select(&links).next().and_then(|a| a.attr("href"));
        if is_assignment || link.and_then(|href| content_id(href, ASSIGNMENT)).is_some() {
            let id = link
                .and_then(|href| content_id(href, ASSIGNMENT))
                .or_else(|| {
                    heading
                        .parent()
                        .and_then(scraper::ElementRef::wrap)
                        .and_then(|parent| parent.attr("id"))
                        .map(str::to_owned)
                })
                .ok_or("无法识别教学网作业编号")?;
            let title = heading
                .text()
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if !title.is_empty() {
                assignments.push((id, title));
            }
        } else if let Some(id) = link.and_then(|href| content_id(href, CONTENT)) {
            folders.push(id);
        }
    }
    Ok((folders, assignments))
}

fn assignment_deadline(bytes: &[u8]) -> Result<Option<DateTime<FixedOffset>>, String> {
    let page = document(bytes)?;
    let text = page
        .select(&selector("#assignMeta2 + div"))
        .next()
        .map(|element| element.text().collect::<String>());
    match text {
        Some(text) if !matches!(text.trim(), "" | "无" | "-" | "--" | "None" | "N/A") => {
            parse_deadline(&text)
                .map(Some)
                .ok_or_else(|| "无法识别教学网作业截止时间".into())
        }
        _ => Ok(None),
    }
}

fn assignment_completed(bytes: &[u8]) -> Result<bool, String> {
    Ok(document(bytes)?
        .select(&selector("#currentAttempt_label"))
        .next()
        .is_some())
}

fn parse_deadline(text: &str) -> Option<DateTime<FixedOffset>> {
    static DATE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
        r"(\d{4})(?:年|-)(\d{1,2})(?:月|-)(\d{1,2})日?\s*(?:(?:星期|周)\S\s*)?(上午|下午)?\s*(\d{1,2})[:：](\d{2})"
    ).unwrap()
    });
    let parts = DATE.captures(text)?;
    let hour: u32 = parts[5].parse().ok()?;
    let hour = match parts.get(4).map(|part| part.as_str()) {
        Some("上午") if (1..=12).contains(&hour) => hour % 12,
        Some("下午") if (1..=12).contains(&hour) => hour % 12 + 12,
        _ => hour,
    };
    let time = NaiveDate::from_ymd_opt(
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
        parts[3].parse().ok()?,
    )?
    .and_hms_opt(hour, parts[6].parse().ok()?, 0)?;
    beijing().from_local_datetime(&time).single()
}

fn beijing() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).unwrap()
}

pub fn recent_cutoff(now: DateTime<Utc>) -> DateTime<FixedOffset> {
    now.with_timezone(&beijing()) - chrono::Duration::days(7)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nested_course_folders_and_assignments_without_reading_descriptions() {
        let page = r#"<ul id="content_listContainer">
          <li><img alt="内容文件夹"><div><h3><a href="/webapps/blackboard/content/listContent.jsp?content_id=_2_1">第三周</a></h3></div></li>
          <li><img alt="作业"><div id="_3_1"><h3><a href="/webapps/assignment/uploadAssignment?content_id=_3_1">阅读 &amp; 写作</a></h3></div><div>作业内容</div></li>
          <li><img alt="文件"><div><h3><a href="https://example.org/webapps/blackboard/content/listContent.jsp?content_id=_4_1">外部链接</a></h3></div></li>
        </ul>"#;
        let (folders, assignments) = folder_entries(page.as_bytes()).unwrap();
        assert_eq!(folders, ["_2_1"]);
        assert_eq!(assignments, [("_3_1".into(), "阅读 & 写作".into())]);
    }

    #[test]
    fn reads_deadline_in_beijing_and_completion_from_attempt_history() {
        let details =
            r#"<h3 id="assignMeta2">截止时间</h3><div>2026年11月20日 星期五 下午11:59</div>"#;
        let deadline = assignment_deadline(details.as_bytes()).unwrap().unwrap();
        assert_eq!(deadline.to_rfc3339(), "2026-11-20T23:59:00+08:00");
        assert_eq!(
            parse_deadline("2026年11月20日 星期五 上午12:05")
                .unwrap()
                .to_rfc3339(),
            "2026-11-20T00:05:00+08:00"
        );
        assert!(assignment_completed(br#"<h3 id="currentAttempt_label">Attempt 1</h3>"#).unwrap());
        assert!(!assignment_completed(b"<h3>Assignment</h3>").unwrap());
    }

    #[test]
    fn prefers_the_current_semester_portlet_to_future_courses() {
        let portal = r#"<div class="portlet"><span class="moduleTitle">当前学期课程</span>
            <a href="/webapps/blackboard/execute/launcher?type=Course&amp;id=PkId{key=_111_1, dataType=course}&amp;url=">code: 当前课(26-27学年第1学期)</a>
          </div><div class="portlet"><span class="moduleTitle">其他课程</span>
            <a href="/webapps/blackboard/execute/launcher?type=Course&id=PkId{key=_222_1, dataType=course}&url=">code: 下学期(26-27学年第2学期)</a>
          </div>"#;
        let courses = current_courses(portal).unwrap();
        assert_eq!(courses.len(), 1);
        assert_eq!(courses[0].key, "_111_1");
    }
}
