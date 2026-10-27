//! Data-only task queue. Producers submit and return; fixed consumers execute work and completion events.
use super::prepared_media::PreparedMedia;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::{mpsc, Mutex, Notify};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompletionTarget {
    None,
    CourseLesson {
        username: String,
        course_id: String,
        lesson_id: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct VideoTask {
    pub id: Uuid,
    pub user_id: Uuid,
    pub priority: u32,
    pub submitted_at: DateTime<Utc>,
    pub media: PreparedMedia,
    pub completion: CompletionTarget,
}

pub struct CompletionEvent {
    pub task_id: Uuid,
    pub user_id: Uuid,
    pub target: CompletionTarget,
    pub result: Result<(), String>,
}

#[async_trait]
pub trait TaskHandler: Send + Sync + 'static {
    async fn process(&self, task: VideoTask) -> Result<(), String>;
    async fn completed(&self, event: CompletionEvent) -> Result<(), String>;
    async fn failed(&self, task_id: Uuid, error: String) {
        tracing::error!(%task_id,%error,"任务完成通知处理失败");
    }
}

struct Entry {
    sequence: u64,
    task: VideoTask,
}
struct Inner {
    pending: Vec<Entry>,
    in_flight: HashSet<Uuid>,
    sequence: u64,
}
pub struct VideoQueue {
    inner: Mutex<Inner>,
    notify: Notify,
    limit: usize,
    started: AtomicBool,
}

impl VideoQueue {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                pending: Vec::new(),
                in_flight: HashSet::new(),
                sequence: 0,
            }),
            notify: Notify::new(),
            limit: limit.max(1),
            started: AtomicBool::new(false),
        })
    }

    pub fn start(self: &Arc<Self>, handler: Arc<dyn TaskHandler>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        // One fixed consumer receives completion events; no submitting task waits for a result.
        let (completed, mut events) = mpsc::channel::<CompletionEvent>(self.limit * 2);
        let queue = self.clone();
        let completion_handler = handler.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let id = event.task_id;
                let result = std::panic::AssertUnwindSafe(completion_handler.completed(event))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| Err("任务完成通知处理器异常终止".into()));
                if let Err(error) = result {
                    completion_handler.failed(id, error).await;
                }
                queue.inner.lock().await.in_flight.remove(&id);
            }
        });
        for _ in 0..self.limit {
            let queue = self.clone();
            let handler = handler.clone();
            let completed = completed.clone();
            tokio::spawn(async move {
                loop {
                    let notified = queue.notify.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    let entry = {
                        let mut inner = queue.inner.lock().await;
                        let next = inner
                            .pending
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, e)| (e.task.priority, e.sequence))
                            .map(|(i, _)| i);
                        next.map(|i| inner.pending.swap_remove(i))
                    };
                    if let Some(entry) = entry {
                        let task = entry.task;
                        let task_id = task.id;
                        let user_id = task.user_id;
                        let target = task.completion.clone();
                        let processor = handler.clone();
                        // Only active worker slots create an execution future; pending entries are plain data.
                        let result = tokio::spawn(async move { processor.process(task).await })
                            .await
                            .unwrap_or_else(|_| Err("视频处理任务异常终止".into()));
                        if completed
                            .send(CompletionEvent {
                                task_id,
                                user_id,
                                target,
                                result,
                            })
                            .await
                            .is_err()
                        {
                            tracing::error!(%task_id, "视频完成事件处理器已停止");
                            return;
                        }
                    } else {
                        notified.await;
                    }
                }
            });
        }
    }

    pub async fn submit(&self, task: VideoTask) -> Result<Uuid, String> {
        if !self.started.load(Ordering::SeqCst) {
            return Err("视频队列尚未启动".into());
        }
        let id = task.id;
        let mut inner = self.inner.lock().await;
        if !inner.in_flight.insert(id) {
            return Err("任务ID已在队列中".into());
        }
        let sequence = inner.sequence;
        inner.sequence += 1;
        inner.pending.push(Entry { task, sequence });
        drop(inner);
        self.notify.notify_one();
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Semaphore;
    struct Handler {
        release: Semaphore,
        started: mpsc::UnboundedSender<Uuid>,
        finished: mpsc::UnboundedSender<(Uuid, bool)>,
        panic_id: Uuid,
    }
    #[async_trait]
    impl TaskHandler for Handler {
        async fn process(&self, task: VideoTask) -> Result<(), String> {
            self.started.send(task.id).unwrap();
            self.release.acquire().await.unwrap().forget();
            if task.id == self.panic_id {
                panic!("test processor panic");
            }
            Ok(())
        }
        async fn completed(&self, event: CompletionEvent) -> Result<(), String> {
            self.finished
                .send((event.task_id, event.result.is_ok()))
                .unwrap();
            Ok(())
        }
    }
    fn task(priority: u32) -> VideoTask {
        VideoTask {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            priority,
            submitted_at: Utc::now(),
            media: PreparedMedia {
                url: "https://example.invalid/video.m3u8".into(),
                enc_key: Some("0123456789abcdef".into()),
            },
            completion: CompletionTarget::None,
        }
    }
    async fn receive<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }
    #[tokio::test]
    async fn submission_returns_id_without_waiter_and_completion_notifies_consumer() {
        let queue = VideoQueue::new(1);
        let (started, mut starts) = mpsc::unbounded_channel();
        let (finished, mut ends) = mpsc::unbounded_channel();
        let handler = Arc::new(Handler {
            release: Semaphore::new(0),
            started,
            finished,
            panic_id: Uuid::nil(),
        });
        queue.start(handler.clone());
        let first = task(5);
        assert_eq!(queue.submit(first.clone()).await.unwrap(), first.id);
        assert_eq!(receive(&mut starts).await, first.id);
        assert!(queue.submit(first.clone()).await.is_err());
        assert!(ends.try_recv().is_err());
        let tasks = vec![task(5), task(1), task(1), task(8)];
        for task in &tasks {
            assert_eq!(queue.submit(task.clone()).await.unwrap(), task.id);
        }
        assert!(starts.try_recv().is_err());
        handler.release.add_permits(5);
        assert_eq!(receive(&mut ends).await, (first.id, true));
        for i in [1, 2, 0, 3] {
            assert_eq!(receive(&mut starts).await, tasks[i].id);
            assert_eq!(receive(&mut ends).await, (tasks[i].id, true));
        }
    }
    #[tokio::test]
    async fn configured_slots_and_failed_completion_do_not_block_later_tasks() {
        let queue = VideoQueue::new(2);
        let (started, mut starts) = mpsc::unbounded_channel();
        let (finished, mut ends) = mpsc::unbounded_channel();
        let tasks = vec![task(5), task(5), task(5), task(5)];
        let handler = Arc::new(Handler {
            release: Semaphore::new(0),
            started,
            finished,
            panic_id: tasks[0].id,
        });
        queue.start(handler.clone());
        for task in &tasks {
            queue.submit(task.clone()).await.unwrap();
        }
        receive(&mut starts).await;
        receive(&mut starts).await;
        assert!(starts.try_recv().is_err());
        handler.release.add_permits(4);
        let mut results = Vec::new();
        for _ in 0..4 {
            results.push(receive(&mut ends).await);
        }
        assert!(results.contains(&(tasks[0].id, false)));
        assert_eq!(results.iter().filter(|(_, ok)| *ok).count(), 3);
        assert_eq!(queue.inner.lock().await.in_flight.len(), 0);
    }

    #[tokio::test]
    async fn completion_errors_and_panics_do_not_stop_notification_consumer() {
        struct CompletionHandler {
            panic_id: Uuid,
            error_id: Uuid,
            results: mpsc::UnboundedSender<(Uuid, bool)>,
        }
        #[async_trait]
        impl TaskHandler for CompletionHandler {
            async fn process(&self, _task: VideoTask) -> Result<(), String> {
                Ok(())
            }
            async fn completed(&self, event: CompletionEvent) -> Result<(), String> {
                if event.task_id == self.panic_id {
                    panic!("test completion panic");
                }
                if event.task_id == self.error_id {
                    return Err("test completion error".into());
                }
                self.results.send((event.task_id, true)).unwrap();
                Ok(())
            }
            async fn failed(&self, id: Uuid, _error: String) {
                self.results.send((id, false)).unwrap();
            }
        }
        let queue = VideoQueue::new(1);
        let tasks = [task(5), task(5), task(5)];
        let (results, mut notifications) = mpsc::unbounded_channel();
        queue.start(Arc::new(CompletionHandler {
            panic_id: tasks[0].id,
            error_id: tasks[1].id,
            results,
        }));
        for task in &tasks {
            queue.submit(task.clone()).await.unwrap();
        }
        assert_eq!(receive(&mut notifications).await, (tasks[0].id, false));
        assert_eq!(receive(&mut notifications).await, (tasks[1].id, false));
        assert_eq!(receive(&mut notifications).await, (tasks[2].id, true));
    }
}
