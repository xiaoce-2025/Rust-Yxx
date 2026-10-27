//! Account-wide, server-owned sequential submission. Does not wait for video processing.
use crate::app_state::AppState;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::FutureExt;
use rand::Rng;
use serde::Serialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

#[async_trait]
pub trait BatchSubmitter: Send + Sync + 'static {
    async fn submit(&self, course_id: &str, video_id: &str) -> Result<Value, String>;
}
pub struct CourseSubmitter {
    pub state: AppState,
    pub user_id: Uuid,
}
#[async_trait]
impl BatchSubmitter for CourseSubmitter {
    async fn submit(&self, course_id: &str, video_id: &str) -> Result<Value, String> {
        super::lesson_submission::submit_lesson(
            self.state.clone(),
            self.user_id,
            course_id.into(),
            video_id.into(),
            None,
        )
        .await
        .map(|axum::Json(value)| value)
        .map_err(|e| e.to_string())
    }
}

#[derive(Clone, Serialize)]
pub struct SubmissionItem {
    pub video_id: String,
    pub status: String,
    pub task_id: Option<Uuid>,
    pub message: Option<String>,
}
#[derive(Clone, Serialize)]
pub struct BatchSnapshot {
    pub id: Uuid,
    pub course_id: String,
    pub active: bool,
    pub status: String,
    pub total: usize,
    pub completed: usize,
    pub submitted: usize,
    pub failed: usize,
    pub started_at: DateTime<Utc>,
    pub estimated_remaining_seconds: u64,
    pub next_submit_at: Option<DateTime<Utc>>,
    pub current_video_id: Option<String>,
    pub results: Vec<SubmissionItem>,
    pub error: Option<String>,
}
struct Record {
    id: Uuid,
    course_id: String,
    status: String,
    items: Vec<SubmissionItem>,
    started_at: DateTime<Utc>,
    delays: Vec<u64>,
    durations: Vec<f64>,
    current: Option<usize>,
    current_started: Option<tokio::time::Instant>,
    next_submit_at: Option<DateTime<Utc>>,
    wait_deadline: Option<tokio::time::Instant>,
    error: Option<String>,
}
impl Record {
    fn snapshot(&self) -> BatchSnapshot {
        let active = self.status == "submitting";
        let submitted = self
            .items
            .iter()
            .filter(|item| item.status == "queued")
            .count();
        let failed = self
            .items
            .iter()
            .filter(|item| item.status == "failed")
            .count();
        let done = submitted + failed;
        let average = if self.durations.is_empty() {
            15.0
        } else {
            (self.durations.iter().sum::<f64>() / self.durations.len() as f64).max(1.0)
        };
        let elapsed = self
            .current_started
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        let wait = self
            .wait_deadline
            .map(|t| {
                t.saturating_duration_since(tokio::time::Instant::now())
                    .as_secs_f64()
            })
            .unwrap_or(0.0);
        let future_delays = self.delays.iter().skip(done).sum::<u64>() as f64;
        let remaining = average * self.items.len().saturating_sub(done) as f64
            - elapsed.min((average - 1.0).max(0.0))
            + wait
            + future_delays;
        BatchSnapshot {
            id: self.id,
            course_id: self.course_id.clone(),
            active,
            status: self.status.clone(),
            total: self.items.len(),
            completed: done,
            submitted,
            failed,
            started_at: self.started_at,
            estimated_remaining_seconds: if active {
                remaining.ceil().max(1.0) as u64
            } else {
                0
            },
            next_submit_at: self.next_submit_at,
            current_video_id: self.current.map(|i| self.items[i].video_id.clone()),
            results: self.items.clone(),
            error: self.error.clone(),
        }
    }
}

pub struct BatchSubmissionManager {
    records: Mutex<HashMap<Uuid, Record>>,
    gates: Mutex<HashMap<Uuid, Arc<RwLock<()>>>>,
}
impl BatchSubmissionManager {
    pub fn new() -> Self {
        Self {
            records: Mutex::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
        }
    }
    pub async fn gate(&self, user: Uuid) -> Arc<RwLock<()>> {
        self.gates
            .lock()
            .await
            .entry(user)
            .or_insert_with(|| Arc::new(RwLock::new(())))
            .clone()
    }
    pub async fn active(&self, user: Uuid) -> bool {
        self.records
            .lock()
            .await
            .get(&user)
            .is_some_and(|r| r.status == "submitting")
    }
    pub async fn snapshot(&self, user: Uuid) -> Option<BatchSnapshot> {
        self.records.lock().await.get(&user).map(Record::snapshot)
    }
    pub async fn start(
        self: &Arc<Self>,
        user: Uuid,
        course_id: String,
        ids: Vec<String>,
        submitter: Arc<dyn BatchSubmitter>,
    ) -> Result<BatchSnapshot, String> {
        if ids.is_empty() {
            return Err("没有需要提交的课节".into());
        }
        let count = ids.len();
        let delays = (0..count - 1)
            .map(|_| rand::thread_rng().gen_range(5..=10))
            .collect();
        let record = Record {
            id: Uuid::new_v4(),
            course_id: course_id.clone(),
            status: "submitting".into(),
            items: ids
                .iter()
                .map(|id| SubmissionItem {
                    video_id: id.clone(),
                    status: "pending".into(),
                    task_id: None,
                    message: None,
                })
                .collect(),
            started_at: Utc::now(),
            delays,
            durations: Vec::new(),
            current: None,
            current_started: None,
            next_submit_at: None,
            wait_deadline: None,
            error: None,
        };
        let id = record.id;
        let snapshot = record.snapshot();
        {
            let mut records = self.records.lock().await;
            if records
                .get(&user)
                .is_some_and(|record| record.status == "submitting")
            {
                return Err("任务正在提交中，请等待当前批量提交完成".into());
            }
            records.insert(user, record);
        }
        let manager = self.clone();
        // One producer owns this account's whole batch; the HTTP request ends immediately.
        tokio::spawn(async move {
            let result =
                std::panic::AssertUnwindSafe(manager.run(user, id, &course_id, ids, submitter))
                    .catch_unwind()
                    .await;
            if result.is_err() {
                manager
                    .finish(
                        user,
                        id,
                        Some("批量提交异常中断，已成功入队的任务继续处理".into()),
                    )
                    .await;
            }
        });
        Ok(snapshot)
    }
    async fn run(
        &self,
        user: Uuid,
        id: Uuid,
        course: &str,
        ids: Vec<String>,
        submitter: Arc<dyn BatchSubmitter>,
    ) {
        for (index, video) in ids.into_iter().enumerate() {
            if index > 0 {
                let delay = {
                    let mut records = self.records.lock().await;
                    let record = records.get_mut(&user).expect("running batch record");
                    let delay = record.delays[index - 1];
                    record.next_submit_at =
                        Some(Utc::now() + chrono::Duration::seconds(delay as i64));
                    record.wait_deadline =
                        Some(tokio::time::Instant::now() + Duration::from_secs(delay));
                    delay
                };
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
            {
                let mut records = self.records.lock().await;
                let record = records.get_mut(&user).expect("running batch record");
                record.current = Some(index);
                record.current_started = Some(tokio::time::Instant::now());
                record.next_submit_at = None;
                record.wait_deadline = None;
                record.items[index].status = "submitting".into();
            }
            let started = tokio::time::Instant::now();
            let result = std::panic::AssertUnwindSafe(submitter.submit(course, &video))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err("课节提交异常".into()))
                .and_then(submitted_task_id);
            {
                let mut records = self.records.lock().await;
                let record = records.get_mut(&user).expect("running batch record");
                record.durations.push(started.elapsed().as_secs_f64());
                record.current = None;
                record.current_started = None;
                match result {
                    Ok(task_id) => {
                        record.items[index].status = "queued".into();
                        record.items[index].task_id = Some(task_id);
                    }
                    Err(error) => {
                        record.items[index].status = "failed".into();
                        record.items[index].message = Some(error);
                    }
                }
            }
        }
        self.finish(user, id, None).await;
    }
    async fn finish(&self, user: Uuid, id: Uuid, error: Option<String>) {
        if let Some(record) = self.records.lock().await.get_mut(&user) {
            if record.id == id {
                record.status = if error.is_some() {
                    "failed"
                } else {
                    "completed"
                }
                .into();
                record.current = None;
                record.current_started = None;
                record.next_submit_at = None;
                record.wait_deadline = None;
                record.error = error;
            }
        }
    }
}
fn submitted_task_id(value: Value) -> Result<Uuid, String> {
    if value["needs_key"] == true {
        return Err("此课节需要手动提供解密密钥，请在批量提交结束后单独处理".into());
    }
    if value["needs_app_login"] == true {
        return Err(value["message"]
            .as_str()
            .unwrap_or("好学 App 需要额外登录验证")
            .into());
    }
    if value["success"] != true {
        return Err(value["message"].as_str().unwrap_or("课节提交失败").into());
    }
    value["job_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| "提交未返回有效任务ID".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    struct Submitter(tokio::sync::mpsc::UnboundedSender<(String, tokio::time::Instant)>);
    #[async_trait]
    impl BatchSubmitter for Submitter {
        async fn submit(&self, _course: &str, video: &str) -> Result<Value, String> {
            self.0
                .send((video.into(), tokio::time::Instant::now()))
                .unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
            if video == "panic" {
                panic!("test submission panic");
            }
            if video == "needs-key" {
                return Ok(json!({"success":true,"needs_key":true}));
            }
            Ok(json!({"success":true,"job_id":Uuid::new_v4()}))
        }
    }
    #[tokio::test(start_paused = true)]
    async fn sequential_submission_survives_failures_and_always_waits_five_to_ten_seconds() {
        let manager = Arc::new(BatchSubmissionManager::new());
        let user = Uuid::new_v4();
        let (tx, mut calls) = tokio::sync::mpsc::unbounded_channel();
        let submitter = Arc::new(Submitter(tx));
        let ids: Vec<_> = ["first", "needs-key", "panic", "last"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let started = manager
            .start(user, "course".into(), ids.clone(), submitter.clone())
            .await
            .unwrap();
        assert!(started.active);
        assert_eq!(started.completed, 0);
        assert!(calls.try_recv().is_err()); // HTTP producer can already return before execution starts.
        let delays = manager.records.lock().await[&user].delays.clone();
        assert!(delays.iter().all(|delay| (5..=10).contains(delay)));
        assert!(manager
            .start(user, "other".into(), vec!["x".into()], submitter)
            .await
            .is_err());
        assert_eq!(manager.snapshot(user).await.unwrap().id, started.id);
        assert!(!manager.active(Uuid::new_v4()).await);
        let mut previous = None;
        for (index, id) in ids.iter().enumerate() {
            let (video, time) = calls.recv().await.unwrap();
            assert_eq!(&video, id);
            if let Some(previous) = previous {
                assert_eq!(
                    time.duration_since(previous),
                    Duration::from_secs(2 + delays[index - 1])
                );
            }
            previous = Some(time);
        }
        tokio::time::advance(Duration::from_secs(2)).await;
        for _ in 0..10 {
            if !manager.active(user).await {
                break;
            }
            tokio::task::yield_now().await;
        }
        let done = manager.snapshot(user).await.unwrap();
        assert!(!done.active);
        assert_eq!(done.completed, 4);
        assert_eq!(done.submitted, 2);
        assert_eq!(done.failed, 2);
        assert_eq!(done.estimated_remaining_seconds, 0);
        assert!(done.results[1]
            .message
            .as_ref()
            .unwrap()
            .contains("解密密钥"));
        assert!(done.results[2].message.as_ref().unwrap().contains("异常"));
        assert!(done.results[3].task_id.is_some());
        assert_eq!(manager.snapshot(user).await.unwrap().id, started.id);
    }
}
