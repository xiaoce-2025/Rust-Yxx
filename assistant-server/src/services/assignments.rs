use chrono::{DateTime, FixedOffset, TimeDelta, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ta::Blackboard;

const CALENDAR_EVENTS: &str = "/webapps/calendar/calendarData/selectedCalendarEvents";

#[derive(Deserialize, Serialize)]
pub struct Reminder {
    pub title: String,
    pub course: String,
    pub deadline: DateTime<Utc>,
}

#[derive(Deserialize)]
struct CalendarAssignment {
    title: String,
    #[serde(rename = "calendarName")]
    course: String,
    #[serde(rename = "endDate")]
    deadline: DateTime<Utc>,
}

pub async fn fetch(client: &Blackboard, now: DateTime<Utc>) -> Result<Vec<Reminder>, String> {
    let (start, end) = calendar_window(now);
    let path = format!(
        "{CALENDAR_EVENTS}?start={}&end={}&course_id=&mode=personal",
        start.timestamp_millis(),
        end.timestamp_millis()
    );
    parse_events(&client.get(&path).await?)
}

fn calendar_window(now: DateTime<Utc>) -> (DateTime<FixedOffset>, DateTime<Utc>) {
    let beijing = FixedOffset::east_opt(8 * 3600).unwrap();
    let effective_day = (now.with_timezone(&beijing) - TimeDelta::hours(4)).date_naive();
    let start = effective_day
        .pred_opt()
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    (
        beijing.from_local_datetime(&start).single().unwrap(),
        now + TimeDelta::weeks(2),
    )
}

fn parse_events(body: &[u8]) -> Result<Vec<Reminder>, String> {
    let events: Vec<Value> =
        serde_json::from_slice(body).map_err(|_| "教学网日历返回了无法识别的数据")?;
    let mut reminders = Vec::new();
    for event in events {
        if !matches!(event["eventType"].as_str(), Some("作业" | "Assignment")) {
            continue;
        }
        let assignment: CalendarAssignment =
            serde_json::from_value(event).map_err(|_| "教学网日历作业数据不完整")?;
        reminders.push(Reminder {
            title: assignment.title,
            course: assignment.course,
            deadline: assignment.deadline,
        });
    }
    reminders.sort_by(|a, b| {
        a.deadline
            .cmp(&b.deadline)
            .then_with(|| a.course.cmp(&b.course))
            .then_with(|| a.title.cmp(&b.title))
    });
    Ok(reminders)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_window_rolls_over_at_four_in_beijing() {
        for (local_now, expected_start) in [
            ("2026-11-20T03:59:59+08:00", "2026-11-18T00:00:00+08:00"),
            ("2026-11-20T04:00:00+08:00", "2026-11-19T00:00:00+08:00"),
        ] {
            let now = local_now.parse::<DateTime<Utc>>().unwrap();
            let (start, end) = calendar_window(now);
            assert_eq!(start.to_rfc3339(), expected_start);
            assert_eq!(end - now, TimeDelta::days(14));
        }
    }

    #[tokio::test]
    async fn fetches_personal_calendar_and_sorts_assignment_deadlines() {
        use axum::{extract::RawQuery, routing::get, Json, Router};
        use serde_json::json;

        let now = "2026-11-20T03:00:00+08:00"
            .parse::<DateTime<Utc>>()
            .unwrap();
        let start = "2026-11-18T00:00:00+08:00"
            .parse::<DateTime<Utc>>()
            .unwrap();
        let expected_query = format!(
            "start={}&end={}&course_id=&mode=personal",
            start.timestamp_millis(),
            (now + TimeDelta::days(14)).timestamp_millis(),
        );
        let app = Router::new().route(
            CALENDAR_EVENTS,
            get(move |RawQuery(query): RawQuery| async move {
                assert_eq!(query.as_deref(), Some(expected_query.as_str()));
                Json(json!([
                    {"eventType":"作业","title":"较晚作业","calendarName":"数学","gradable":false,
                     "endDate":"2026-11-21T15:59:00.000Z"},
                    {"eventType":"个人活动","title":"日程"},
                    {"eventType":"作业","title":"较早作业","calendarName":"计算机","gradable":true,
                     "startDate":"2026-11-20T12:00:00.000Z","endDate":"2026-11-20T14:00:00.000Z"}
                ]))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Blackboard::test_client(&format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let reminders = fetch(&client, now).await.unwrap();
        server.abort();
        assert_eq!(reminders.len(), 2);
        assert_eq!(reminders[0].title, "较早作业");
        assert_eq!(reminders[0].course, "计算机");
        assert_eq!(
            reminders[0].deadline.to_rfc3339(),
            "2026-11-20T14:00:00+00:00"
        );
        assert_eq!(reminders[1].title, "较晚作业");
        assert!(parse_events(b"<html>login</html>").is_err());
    }
}
