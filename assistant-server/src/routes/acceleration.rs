use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    services::{
        acceleration::{AccelerationSettings, ACCELERATED_PRIORITY},
        pkuvideo::PipelineStatus,
        video_queue::QueueWait,
    },
    store::acceleration::{storage_error, AccelerationWallet},
};
use axum::{
    extract::{Path, State},
    http::header::CACHE_CONTROL,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use uuid::Uuid;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/acceleration/tickets", get(wallet).post(buy))
        .route(
            "/api/video-task-history/:id/acceleration",
            get(estimate).post(redeem),
        )
}

pub(super) async fn queue_waits(
    state: &AppState,
    user: Uuid,
) -> Result<HashMap<Uuid, QueueWait>, ApiError> {
    let settings = state.store.acceleration_settings().await?;
    Ok(state
        .video_queue
        .estimated_waits(user, settings.processing_time)
        .await)
}

async fn wallet(
    State(state): State<AppState>,
    user: CurrentUser,
) -> Result<impl IntoResponse, ApiError> {
    let wallet = state.store.acceleration_wallet(user.id).await?;
    let settings = state.store.acceleration_settings().await?;
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(
            json!({"tickets": wallet.tickets, "balance": wallet.balance, "price": settings.price}),
        ),
    ))
}

#[derive(Deserialize)]
struct Purchase {
    request_id: Uuid,
    expected_price: i64,
}

async fn buy(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(input): Json<Purchase>,
) -> Result<Json<AccelerationWallet>, ApiError> {
    Ok(Json(
        state
            .store
            .buy_acceleration_ticket(user.id, input.request_id, input.expected_price)
            .await?,
    ))
}

async fn estimate(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let eligible = state
        .pkuvideo_jobs
        .read()
        .await
        .get(&id)
        .filter(|job| job.user_id == user.id)
        .map(|job| job.status == PipelineStatus::Pending)
        .ok_or(ApiError::NotFound)?;
    let conflict = || ApiError::Conflict("任务已离开队列或已加速，请刷新历史记录".into());
    if !eligible {
        return Err(conflict());
    }
    let settings = state.store.acceleration_settings().await?;
    let estimate = state
        .video_queue
        .estimate_acceleration(user.id, id, settings.processing_time)
        .await
        .ok_or_else(conflict)?;
    Ok(([(CACHE_CONTROL, "no-store")], Json(estimate)))
}

async fn redeem(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<Uuid>,
    Json(()): Json<()>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Finish the durable debit and in-memory update even if the client disconnects.
    tokio::spawn(async move {
        state
            .video_queue
            .accelerate(user.id, id, async {
                let mut jobs = state.pkuvideo_jobs.write().await;
                let job = jobs
                    .get_mut(&id)
                    .filter(|job| job.user_id == user.id)
                    .ok_or(ApiError::NotFound)?;
                if job.status != PipelineStatus::Pending {
                    return Err(ApiError::Conflict("只有排队中的任务可以使用加速券".into()));
                }
                let updated_at = state.store.redeem_acceleration_ticket(user.id, id).await?;
                job.priority = ACCELERATED_PRIORITY;
                job.acceleration_used = true;
                job.updated_at = updated_at;
                Ok(())
            })
            .await
    })
    .await
    .map_err(storage_error)??;
    Ok(Json(json!({"priority": ACCELERATED_PRIORITY})))
}

pub(super) async fn admin_get(
    State(state): State<AppState>,
) -> Result<Json<AccelerationSettings>, ApiError> {
    Ok(Json(state.store.acceleration_settings().await?))
}

pub(super) async fn admin_save(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(settings): Json<AccelerationSettings>,
) -> Result<Json<AccelerationSettings>, ApiError> {
    if !settings.valid() {
        return Err(ApiError::BadRequest(
            "请检查售价、任务耗时上下界、均值和方差。均值须位于上下界之间。".into(),
        ));
    }
    state
        .store
        .save_acceleration_settings(user.id, settings)
        .await?;
    Ok(Json(settings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Settings,
        services::{
            pkuvideo::PkuVideoJob,
            prepared_media::PreparedMedia,
            video_jobs::enqueue_prepared_job,
            video_queue::{CompletionEvent, TaskHandler, VideoQueue, VideoTask},
        },
        store::{FileStore, FileUser, FileUserSession},
    };
    use chrono::{Duration, Utc};
    use std::sync::Arc;
    use tokio::sync::{mpsc, Semaphore};

    struct PausedWorker {
        started: mpsc::UnboundedSender<(Uuid, u32)>,
        release: Semaphore,
    }

    #[async_trait::async_trait]
    impl TaskHandler for PausedWorker {
        async fn process(&self, task: VideoTask) -> Result<(), String> {
            self.started.send((task.id, task.priority)).unwrap();
            self.release.acquire().await.unwrap().forget();
            Ok(())
        }

        async fn completed(&self, _: CompletionEvent) -> Result<(), String> {
            Ok(())
        }
    }

    async fn next_start(starts: &mut mpsc::UnboundedReceiver<(Uuid, u32)>) -> (Uuid, u32) {
        tokio::time::timeout(std::time::Duration::from_secs(5), starts.recv())
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn acceleration_purchase_and_redemption_reorder_only_one_pending_job() {
        let temp = tempdir::TempDir::new("acceleration").unwrap();
        let store = Arc::new(FileStore::new(temp.path().into()).await.unwrap());
        let owner = Uuid::new_v4();
        let outsider = Uuid::new_v4();
        let tokens = [Uuid::new_v4(), Uuid::new_v4()];
        let now = Utc::now();
        for (id, token) in [(owner, tokens[0]), (outsider, tokens[1])] {
            store
                .insert_user(FileUser {
                    id,
                    username: id.to_string(),
                    email: format!("{id}@example.invalid"),
                    password_hash: String::new(),
                    is_active: true,
                    created_at: now,
                    updated_at: now,
                    storage_quota: 3,
                    processing_quota: 4,
                    processing_priority: 1,
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
                    created_at: now,
                    user_agent: None,
                    ip_address: None,
                })
                .await
                .unwrap();
        }
        let settings: Settings = serde_json::from_value(json!({
            "admin":{"user_ids":[owner]},
            "server":{"address":"127.0.0.1","port":0},
            "wechat":{"api_base_url":"","oauth_app_id":"","oauth_app_secret":"","redirect_uri":"","qr_bot_type":""},
            "security":{"session_ttl_hours":1},"cors":{"allowed_origins":[],"allow_credentials":false},
            "audioapi_paraformer":{},"llm_deepseek":{}
        })).unwrap();
        let mut state = AppState::new(settings, store.clone());
        let (started, mut starts) = mpsc::unbounded_channel();
        let worker = Arc::new(PausedWorker {
            started,
            release: Semaphore::new(0),
        });
        state.video_queue = VideoQueue::new(1);
        state.video_queue.start(worker.clone());
        let mut ids = Vec::new();
        for _ in 0..3 {
            let id = Uuid::new_v4();
            let url = format!("https://example.invalid/{id}");
            let job = PkuVideoJob::new(id, owner, url.clone(), String::new());
            store.insert_pkuvideo_job(job.clone()).await.unwrap();
            state.pkuvideo_jobs.write().await.insert(id, job);
            enqueue_prepared_job(&state, id, owner, PreparedMedia { url, enc_key: None })
                .await
                .unwrap();
            ids.push(id);
        }
        assert_eq!(next_start(&mut starts).await, (ids[0], 4));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::routes::router(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        store
            .save_sidecar(
                std::path::Path::new("acceleration_settings"),
                &json!({
                    "price":2, "normal":{"min":8,"max":8,"mean":8,"variance":1},
                    "accelerated":{"min":3,"max":3,"mean":3,"variance":1}
                })
                .to_string(),
            )
            .await
            .unwrap();
        let configure = |token| {
            client
                .post(format!("{base}/api/admin/acceleration"))
                .bearer_auth(token)
                .header("X-Admin-Request", "1")
                .json(&json!({
                    "price":2, "processing_time":{"min":12,"max":12,"mean":12,"variance":0}
                }))
        };
        assert_eq!(configure(tokens[1]).send().await.unwrap().status(), 403);
        let legacy: serde_json::Value = client
            .get(format!("{base}/api/admin/acceleration"))
            .bearer_auth(tokens[0])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            legacy,
            json!({"price":2,"processing_time":{"min":8,"max":8,"mean":8.0,"variance":1.0}})
        );

        let purchase = client
            .post(format!("{base}/api/acceleration/tickets"))
            .bearer_auth(tokens[0])
            .json(&json!({"request_id":Uuid::new_v4(),"expected_price":2}));
        for _ in 0..2 {
            let wallet: serde_json::Value = purchase
                .try_clone()
                .unwrap()
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(wallet, json!({"balance":2,"tickets":1}));
        }
        assert_eq!(configure(tokens[0]).send().await.unwrap().status(), 200);
        let job_url = |id| format!("{base}/api/video-task-history/{id}/acceleration");
        assert_eq!(
            client
                .get(job_url(ids[2]))
                .bearer_auth(tokens[1])
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        let estimate: serde_json::Value = client
            .get(job_url(ids[2]))
            .bearer_auth(tokens[0])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            estimate,
            json!({"current_minutes":24,"accelerated_minutes":12,"saved_minutes":12})
        );
        let history = || {
            client
                .get(format!("{base}/api/video-task-history"))
                .bearer_auth(tokens[0])
        };
        let records: Vec<serde_json::Value> = history().send().await.unwrap().json().await.unwrap();
        let queued = records
            .iter()
            .find(|job| job["id"] == json!(ids[2]))
            .unwrap();
        assert_eq!(
            queued["estimated_wait_minutes"],
            estimate["current_minutes"]
        );
        let detail: serde_json::Value = client
            .get(format!("{base}/api/pkuvideo/jobs/{}", ids[2]))
            .bearer_auth(tokens[0])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            detail["estimated_wait_minutes"],
            estimate["current_minutes"]
        );
        let redeem = |id| client.post(job_url(id)).bearer_auth(tokens[0]).json(&());
        assert_eq!(redeem(ids[0]).send().await.unwrap().status(), 409);
        assert_eq!(store.acceleration_wallet(owner).await.unwrap().tickets, 1);
        let (a, b) = tokio::join!(redeem(ids[2]).send(), redeem(ids[2]).send());
        let mut statuses = [a.unwrap().status().as_u16(), b.unwrap().status().as_u16()];
        statuses.sort();
        assert_eq!(statuses, [200, 409]);
        let records: Vec<serde_json::Value> = history().send().await.unwrap().json().await.unwrap();
        let new_front = records
            .iter()
            .find(|job| job["id"] == json!(ids[2]))
            .unwrap();
        assert_eq!(
            new_front["estimated_wait_minutes"],
            estimate["accelerated_minutes"]
        );
        assert_eq!(new_front["can_accelerate"], false);
        assert_eq!(redeem(ids[1]).send().await.unwrap().status(), 400);
        assert_eq!(store.acceleration_wallet(owner).await.unwrap().tickets, 0);
        let persisted = FileStore::new(temp.path().into()).await.unwrap();
        assert_eq!(
            persisted.get_pkuvideo_job(ids[1]).await.unwrap().priority,
            4
        );
        let accelerated = persisted.get_pkuvideo_job(ids[2]).await.unwrap();
        assert!(accelerated.acceleration_used);
        assert_eq!(accelerated.priority, 2);
        worker.release.add_permits(3);
        assert_eq!(next_start(&mut starts).await, (ids[2], 2));
        assert_eq!(next_start(&mut starts).await, (ids[1], 4));
        server.abort();
    }
}
