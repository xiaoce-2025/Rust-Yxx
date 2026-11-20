use axum::{
    extract::{FromRequestParts, Path, Query, Request, State},
    http::{header::CACHE_CONTROL, HeaderValue, Method},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{app_state::AppState, auth::CurrentUser, error::ApiError};
#[path = "admin_ta_ai.rs"]
mod ta_ai;
#[path = "admin_operations.rs"]
mod operations;
#[path = "admin_operations_view.rs"]
mod operations_view;
#[path = "admin_tex_cache.rs"]
mod tex_cache;

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/admin", get(page))
        .route("/admin/", get(page))
        .route("/api/admin/data", get(management_data))
        .route("/api/admin/summary", get(management_summary))
        .route("/api/admin/list", get(management_list))
        .route("/api/admin/related", get(management_related))
        .route("/api/admin/whitelist/change", post(change_whitelist))
        .route("/api/admin/users/:id", post(update_user))
        .route("/api/admin/users/:id/note-credits/grant",post(grant_note_credits))
        .route("/api/admin/users/:id/note-credits/ledger",get(note_credit_ledger))
        .route("/api/admin/users/:id/revoke-sessions", post(revoke_sessions))
        .route("/api/admin/whitelist", post(save_whitelist))
        .route("/api/admin/ta-ai", get(ta_ai::get).post(ta_ai::save))
        .route("/api/admin/records", get(operations::records))
        .route("/api/admin/records/:id", get(operations::ai_record))
        .route("/api/admin/notes", get(operations::notes))
        .route("/api/admin/note", get(operations::note))
        .route("/api/admin/features", get(operations::features).post(operations::save_features))
        .route("/api/admin/acceleration", get(super::acceleration::admin_get).post(super::acceleration::admin_save))
        .route("/api/admin/tex-cache", get(tex_cache::list))
        .route("/api/admin/tex-cache/:key", get(tex_cache::detail))
        .route("/api/admin/tex-cache/:key/tex", get(tex_cache::tex))
        .route_layer(middleware::from_fn_with_state(state, require_admin))
}

async fn require_admin(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let page = request.uri().path().starts_with("/admin");
    let (mut parts, body) = request.into_parts();
    let result = CurrentUser::from_request_parts(&mut parts, &state).await;
    let mut response = match result {
        Err(_) if page => Redirect::to("/auth?next=%2Fadmin").into_response(),
        Err(error) => error.into_response(),
        Ok(user) if !state.settings.admin.user_ids.contains(&user.id) => {
            if page {
                (axum::http::StatusCode::FORBIDDEN, Html("<!doctype html><html lang=zh-CN><meta charset=utf-8><title>无权访问</title><body style='font:16px Arial;color:#2c2020;background:#fff8f5;padding:10%'><h1>此账户没有后台访问权限</h1><p>请联系服务管理员配置账户权限。</p><a href='/'>返回首页</a></body></html>")).into_response()
            } else { ApiError::Forbidden("需要管理员权限".into()).into_response() }
        }
        Ok(user) => {
            // Browsers cannot submit this custom header through a cross-site form.
            if parts.method != Method::GET && parts.headers.get("x-admin-request").and_then(|h| h.to_str().ok()) != Some("1") {
                ApiError::Forbidden("缺少后台请求校验标识".into()).into_response()
            } else {
                parts.extensions.insert(user);
                next.run(Request::from_parts(parts, body)).await
            }
        }
    };
    response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn page() -> Html<String> { Html(tex_cache::page(operations_view::page(ta_ai::page()))) }

async fn management_summary(current: CurrentUser) -> Json<Value> {
    Json(json!({"current_user_id":current.id,"username":current.username}))
}

async fn management_list(
    State(state): State<AppState>, Query(query): Query<crate::store::AdminList>,
) -> Result<Json<Value>, ApiError> {
    if !matches!(query.model.as_str(), "users" | "whitelist" | "notes" | "replay" | "translation")
        || query.q.chars().count() > 300
        || query.id.as_ref().is_some_and(|id| id.len() > 254)
    {
        return Err(ApiError::BadRequest("无效的管理列表参数".into()));
    }
    let value = state.store.admin_list(query, &state.settings.admin.user_ids).await
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    if value.is_null() { return Err(ApiError::NotFound); }
    Ok(Json(value))
}

#[derive(Deserialize)]
struct RelatedQuery {
    #[serde(default)] users: String,
    #[serde(default)] jobs: String,
}

async fn management_related(State(state): State<AppState>, Query(q): Query<RelatedQuery>) -> Result<Json<Value>, ApiError> {
    let parse = |value: &str| {
        let ids: Vec<Uuid> = value.split(',').take(201).filter_map(|id| id.parse().ok()).collect();
        (ids.len() <= 200).then_some(ids).ok_or(ApiError::BadRequest("无效的关联记录".into()))
    };
    state.store.admin_related(&parse(&q.users)?, &parse(&q.jobs)?).await
        .map(Json).map_err(|error| ApiError::Internal(error.to_string()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WhitelistChange {
    old: Option<String>,
    email: Option<String>,
    #[serde(default)] remove: Vec<String>,
}

async fn change_whitelist(State(state): State<AppState>, Json(mut input): Json<WhitelistChange>) -> Result<Json<Value>, ApiError> {
    input.email = input.email.map(|email| email.trim().into());
    let valid = |email: &str| email.len() <= 254 && !email.chars().any(char::is_whitespace)
        && email.split('@').count() == 2 && !email.starts_with('@') && !email.ends_with('@');
    if input.email.as_deref().is_some_and(|email| !valid(email))
        || input.old.as_ref().is_some_and(|email| email.len() > 254)
        || input.remove.len() > 200 || input.remove.iter().any(|email| email.len() > 254)
        || (input.email.is_some() && !input.remove.is_empty())
        || (input.email.is_none() && (input.remove.is_empty() || input.old.is_some()))
    {
        return Err(ApiError::BadRequest("无效的白名单操作".into()));
    }
    state.store.change_whitelist(input.old, input.email, input.remove).await?;
    Ok(Json(json!({"success":true})))
}


async fn management_data(State(state): State<AppState>, current: CurrentUser) -> Result<Json<Value>,ApiError> {
    let last_logins=state.store.last_logins().await.map_err(|e|ApiError::Internal(e.to_string()))?;
    let counts = state.store.admin_session_counts().await;
    let mut users = state.store.admin_users().await;
    users.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
    let users: Vec<Value> = users.into_iter().map(|user| json!({
        "id": user.id, "username": user.username, "email": user.email,
        "is_active": user.is_active, "created_at": user.created_at,
        "last_login_at": last_logins.get(&user.id.to_string()),"updated_at":user.updated_at,
        "storage_quota": user.storage_quota, "processing_quota": user.processing_quota,
        "processing_priority": user.processing_priority, "sessions": counts.get(&user.id).copied().unwrap_or(0),
        "is_admin": state.settings.admin.user_ids.contains(&user.id)
    })).collect();
    // Deliberately project only operational metadata: no credentials, URLs or file paths.
    let mut jobs = Vec::new();
    for job in state.store.get_all_pkuvideo_jobs().await.map_err(|error| ApiError::Internal(error.to_string()))?.values() {
        jobs.push(json!({"id":job.id,"user_id":job.user_id,"kind":"notes","course_name":job.course_name,"course_date":job.course_date,"regenerate":job.regenerate,"result_source":job.result_source,"url_key":job.url_key,"status":job.status,"created_at":job.created_at,"updated_at":job.updated_at,"billing_state":job.billing_state,"charged_units":job.charged_units,"billing_duration_seconds":job.billing_duration_seconds,"failure_code":job.failure_code}));
    }
    for job in state.store.get_all_pkuvideo_fw_jobs().await.map_err(|error| ApiError::Internal(error.to_string()))?.values() {
        jobs.push(json!({"id":job.id,"user_id":job.user_id,"kind":"replay","status":job.status,"created_at":job.created_at,"updated_at":job.updated_at}));
    }
    let entries: Vec<_> = state.video_translations.entries.lock().await.values().cloned().collect();
    for entry in entries {
        let job = entry.job.lock().await;
        jobs.push(json!({"id":job.id,"user_id":job.user_id,"kind":"translation","status":job.status,"updated_at":job.updated_at}));
    }
    jobs.sort_by(|a, b| b["updated_at"].as_str().cmp(&a["updated_at"].as_str()));
    Ok(Json(json!({"current_user_id":current.id,"username":current.username,"users":users,"jobs":jobs,
        "whitelist":state.store.admin_whitelist().await})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserUpdate { is_active: bool, storage_quota: i32, #[serde(default, rename = "processing_quota")] _processing_quota: Option<i64>, processing_priority: u32 }

async fn update_user(State(state): State<AppState>, Path(id): Path<Uuid>, audit: Option<axum::Extension<super::operations::AuditId>>, Json(input): Json<UserUpdate>) -> Result<Json<Value>, ApiError> {
    if !input.is_active && state.settings.admin.user_ids.contains(&id) {
        return Err(ApiError::BadRequest("不能停用管理员账户，请先在服务配置中移除其管理员权限".into()));
    }
    if input.storage_quota < 0 || !(1..=100).contains(&input.processing_priority) {
        return Err(ApiError::BadRequest("配额必须为非负整数，优先级范围为 1–100".into()));
    }
    state.store.admin_update_user(id, input.is_active, input.storage_quota, input.processing_priority, audit.map(|axum::Extension(id)| id.0)).await?;
    Ok(Json(json!({"success":true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreditGrant { amount:i64, reason:String, idempotency_key:String }
async fn grant_note_credits(State(state):State<AppState>,current:CurrentUser,Path(id):Path<Uuid>,audit:Option<axum::Extension<super::operations::AuditId>>,Json(input):Json<CreditGrant>)->Result<Json<Value>,ApiError> {
    if input.amount<=0 || input.reason.trim().is_empty() || input.reason.len()>1000 || input.idempotency_key.is_empty() || input.idempotency_key.len()>128 {
        return Err(ApiError::BadRequest("请填写正整数的希元点数、发放原因和幂等标识".into()));
    }
    state.store.get_user(id).await.ok_or(ApiError::NotFound)?;
    let value=state.store.grant_note_credits(id,current.id,input.amount,&input.reason,&input.idempotency_key,audit.map(|axum::Extension(id)| id.0)).await.map_err(|e| {
        if matches!(e.to_string().as_str(),"Overflow"|"IdempotencyConflict"|"InvalidGrant") {ApiError::BadRequest(e.to_string())} else {ApiError::Internal(e.to_string())}
    })?;
    Ok(Json(value))
}
async fn note_credit_ledger(State(state):State<AppState>,Path(id):Path<Uuid>,axum::extract::Query(query):axum::extract::Query<super::note_credits::LedgerQuery>)->Result<Json<Value>,ApiError> {
    state.store.get_user(id).await.ok_or(ApiError::NotFound)?;
    Ok(Json(state.store.note_ledger(id,query.limit.unwrap_or(30),query.cursor.as_deref()).await.map_err(|e|ApiError::Internal(e.to_string()))?))
}

async fn revoke_sessions(State(state): State<AppState>, current: CurrentUser, Path(id): Path<Uuid>, audit: Option<axum::Extension<super::operations::AuditId>>) -> Result<Json<Value>, ApiError> {
    if id == current.id { return Err(ApiError::BadRequest("请通过退出登录结束自己的会话".into())); }
    state.store.get_user(id).await.ok_or(ApiError::NotFound)?;
    let count = state.store.admin_revoke_sessions(id, audit.map(|axum::Extension(id)| id.0)).await.map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({"success":true,"revoked":count})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WhitelistUpdate { emails: Vec<String> }

async fn save_whitelist(State(state): State<AppState>, Json(input): Json<WhitelistUpdate>) -> Result<Json<Value>, ApiError> {
    let mut emails: Vec<String> = input.emails.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if emails.len() > 10000 || emails.iter().any(|s| s.len() > 254 || s.chars().any(char::is_whitespace) || s.split('@').count() != 2 || s.starts_with('@') || s.ends_with('@')) {
        return Err(ApiError::BadRequest("请填写有效邮箱，每行一个，最多 10000 个".into()));
    }
    emails.sort(); emails.dedup();
    state.store.admin_save_whitelist(emails).await.map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({"success":true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Settings, store::{FileStore, FileUser, FileUserSession}};
    use chrono::{Duration, Utc};
    use std::sync::Arc;

    #[tokio::test]
    async fn admin_access_mutations_and_persistence() {
        let temp = tempdir::TempDir::new("admin").unwrap();
        let admin = Uuid::new_v4();
        let member = Uuid::new_v4();
        let settings: Settings = serde_json::from_value(json!({
            "admin":{"user_ids":[admin]},
            "server":{"address":"127.0.0.1","port":0},
            "wechat":{"api_base_url":"","oauth_app_id":"","oauth_app_secret":"","redirect_uri":"","qr_bot_type":""},
            "security":{"session_ttl_hours":1},"cors":{"allowed_origins":[],"allow_credentials":false},
            "audioapi_paraformer":{},"llm_deepseek":{}
        })).unwrap();
        let store = Arc::new(FileStore::new(temp.path().into()).await.unwrap());
        std::fs::create_dir_all("./course_data").unwrap();
        let note_dir=tempdir::TempDir::new_in("./course_data","admin-notes").unwrap();
        let note_username=note_dir.path().file_name().unwrap().to_str().unwrap().to_owned();
        let state = AppState::new(settings, store.clone());
        let mut tokens = Vec::new();
        for id in [admin, member] {
            store.insert_user(FileUser {
                id, username: id.to_string(), email: format!("{}@example.com",if id==member{note_username.clone()}else{id.to_string()}), password_hash:"SECRET_HASH".into(),
                is_active:true, created_at:Utc::now(), updated_at:Utc::now(), processing_priority:5,
                storage_quota:3, processing_quota:1, iaaa_encrypted_password:Some("SECRET_IAAA".into()), iaaa_public_key:None,
            }).await.unwrap();
            let token = Uuid::new_v4(); tokens.push(token);
            store.insert_session(FileUserSession {
                session_id:token,user_id:id,expires_at:Utc::now()+Duration::hours(1),
                created_at:Utc::now(),user_agent:None,ip_address:None,
            }).await.unwrap();
        }
        let private_id=Uuid::new_v4();
        let mut private_job=crate::services::pkuvideo::PkuVideoJob::new(private_id,member,"https://private.invalid/?token=SECRET_TOKEN".into(),"SECRET_LOCAL_PATH".into());
        private_job.status=crate::services::pkuvideo::PipelineStatus::Failed;
        private_job.steps[0].status=crate::services::pkuvideo::PipelineStatus::Failed;
        private_job.steps[0].message="SECRET_INTERNAL_ERROR".into();
        private_job.error=Some("SECRET_ERROR".into());
        store.insert_pkuvideo_job(private_job.clone()).await.unwrap();
        state.pkuvideo_jobs.write().await.insert(private_id,private_job);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}",listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener,crate::routes::router(state)).await.unwrap(); });
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let private_url=format!("{base}/api/pkuvideo/jobs/{private_id}");
        assert_eq!(client.get(&private_url).bearer_auth(tokens[0]).send().await.unwrap().status(),403);
        let safe_job=client.get(&private_url).bearer_auth(tokens[1]).send().await.unwrap().text().await.unwrap();
        assert!(!safe_job.contains("SECRET_") && !safe_job.contains("private.invalid"));
        let list_url=format!("{base}/api/admin/list?model=users&role=member&page=99");
        assert_eq!(client.get(&list_url).bearer_auth(tokens[1]).send().await.unwrap().status(),403);
        let list: Value=client.get(&list_url).bearer_auth(tokens[0]).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert_eq!(list["total"],1);
        assert_eq!(list["page"],1);
        assert_eq!(list["records"][0]["id"],member.to_string());
        let note_url=format!("{base}/api/admin/list?model=notes&id={private_id}");
        let note: Value=client.get(&note_url).bearer_auth(tokens[0]).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert_eq!(note["id"],private_id.to_string());
        assert!(!note.to_string().contains("SECRET_"));
        let whitelist_url=format!("{base}/api/admin/list?model=whitelist&q=unknown");
        let empty:Value=client.get(&whitelist_url).bearer_auth(tokens[0]).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert_eq!(empty["total"],0);
        let cache_key=crate::store::tex_hash(b"shared-classroom-url");
        store.publish_tex(&cache_key,b"\\section{Shared classroom}",Uuid::new_v4()).await.unwrap();
        for path in ["/api/admin/tex-cache".to_string(),format!("/api/admin/tex-cache/{cache_key}"),format!("/api/admin/tex-cache/{cache_key}/tex")] {
            let url=format!("{base}{path}");
            assert_eq!(client.get(&url).send().await.unwrap().status(),401);
            assert_eq!(client.get(&url).bearer_auth(tokens[1]).send().await.unwrap().status(),403);
            let response=client.get(&url).bearer_auth(tokens[0]).send().await.unwrap();
            assert_eq!(response.status(),200);
            let text=response.text().await.unwrap();
            assert!(!text.contains("tex_path") && !text.contains("global_tex_cache"));
        }
        let cache_page=client.get(format!("{base}/admin")).bearer_auth(tokens[0]).send().await.unwrap().text().await.unwrap();
        assert!(cache_page.contains("data-model=\"global_tex\""));
        assert!(cache_page.contains("else if (model==='global_tex') renderGlobalTex()"));
        let flags_url=format!("{base}/api/admin/features");
        assert_eq!(client.get(&flags_url).send().await.unwrap().status(),401);
        assert_eq!(client.get(&flags_url).bearer_auth(tokens[1]).send().await.unwrap().status(),403);
        assert_eq!(client.post(&flags_url).bearer_auth(tokens[0]).json(&json!({"show_reprocess":false})).send().await.unwrap().status(),403);
        assert_eq!(client.post(&flags_url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&json!({"show_reprocess":false})).send().await.unwrap().status(),200);
        let flags:Value=client.get(format!("{base}/api/site/features")).bearer_auth(tokens[1]).send().await.unwrap().json().await.unwrap();assert_eq!(flags["show_reprocess"],false);
        // Disabling the control also blocks a direct request, before touching teaching data.
        assert_eq!(client.post(format!("{base}/api/pkuvideofullclass/courses/_c/videos/_v/reprocess")).bearer_auth(tokens[1]).send().await.unwrap().status(),403);
        let course="_admin_course";
        std::fs::create_dir_all(note_dir.path().join(course)).unwrap();
        std::fs::write(note_dir.path().join(course).join("lesson.tex"),"PRIVATE_NOTE_BODY").unwrap();
        let lesson=crate::services::lesson_storage::LessonInfo{ initial_tex_hash: None,enc_key:None,id:"_l_1".into(),video_url:"SECRET_VIDEO_URL".into(),play_url:String::new(),date:"2026-11-14".into(),time:String::new(),day_of_week:String::new(),is_processed:true,tex_filename:Some("lesson.tex".into()),manual_replay:None};
        crate::services::lesson_storage::save_lessons(&store,&note_username,course,&[lesson]).await.unwrap();
        let inventory_url=format!("{base}/api/admin/notes?user={member}");
        assert_eq!(client.get(&inventory_url).bearer_auth(tokens[1]).send().await.unwrap().status(),403);
        let inventory:Value=client.get(&inventory_url).bearer_auth(tokens[0]).send().await.unwrap().json().await.unwrap();assert_eq!(inventory["total"],1);
        assert!(!inventory.to_string().contains("PRIVATE_NOTE_BODY")&&!inventory.to_string().contains("SECRET_VIDEO_URL"));
        let note_url=format!("{base}/api/admin/note?owner={member}&course={course}&lesson=_l_1&format=tex");
        assert_eq!(client.get(&note_url).bearer_auth(tokens[1]).send().await.unwrap().status(),403);
        let note=client.get(format!("{note_url}&ignored=SECRET_QUERY")).bearer_auth(tokens[0]).send().await.unwrap();
        assert_eq!(note.headers()["cache-control"],"no-store");
        let note_audit = note.headers()["x-audit-id"].to_str().unwrap().parse::<i64>().unwrap();
        assert_eq!(note.text().await.unwrap(),"PRIVATE_NOTE_BODY");
        assert_eq!(client.get(format!("{base}/api/admin/note?owner={member}&course=..&lesson=_l_1&format=tex")).bearer_auth(tokens[0]).send().await.unwrap().status(),400);
        let audit_url=format!("{base}/api/admin/records?kind=activity&q=note.read");
        assert_eq!(client.get(&audit_url).bearer_auth(tokens[1]).send().await.unwrap().status(),403);
        let reads:Value=client.get(&audit_url).bearer_auth(tokens[0]).send().await.unwrap().json().await.unwrap();
        assert!(reads["records"].as_array().unwrap().iter().any(|r|r["user_id"]==admin.to_string()&&r["status"]==200));
        assert!(!reads.to_string().contains("PRIVATE_NOTE_BODY")&&!reads.to_string().contains(&tokens[0].to_string()));
        let exact: Value = client.get(format!("{base}/api/admin/records?kind=activity&q=%23{note_audit}&user={member}&event=note.read&status=200"))
            .bearer_auth(tokens[0]).send().await.unwrap().json().await.unwrap();
        assert_eq!(exact["total"], 1);
        assert_eq!(exact["records"][0]["id"], note_audit);
        assert_eq!(exact["records"][0]["subject_id"], member.to_string());
        assert_eq!(exact["records"][0]["detail"]["lesson"], "_l_1");
        assert!(!exact.to_string().contains("SECRET_QUERY"));
        assert_eq!(client.get(format!("{base}/admin")).send().await.unwrap().status(),303);
        assert_eq!(client.get(format!("{base}/api/admin/data")).send().await.unwrap().status(),401);
        for path in ["/admin","/api/admin/data"] {
            assert_eq!(client.get(format!("{base}{path}")).bearer_auth(tokens[1]).send().await.unwrap().status(),403);
        }
        let page = client.get(format!("{base}/admin")).bearer_auth(tokens[0]).send().await.unwrap();
        assert_eq!(page.status(),200); assert_eq!(page.headers()[CACHE_CONTROL],"no-store");
        let rendered = page.text().await.unwrap();
        assert!(rendered.contains("data-model=\"credits\"") && rendered.contains("applyOperationsLink(); if (data)"));
        let snapshot = client.get(format!("{base}/api/admin/data")).bearer_auth(tokens[0]).send().await.unwrap().text().await.unwrap();
        assert!(!snapshot.contains("SECRET_")); assert!(!snapshot.contains(&tokens[0].to_string()));
        assert_eq!(serde_json::from_str::<Value>(&snapshot).unwrap()["users"].as_array().unwrap().len(),2);
        let url = format!("{base}/api/admin/users/{member}");
        let update = json!({"is_active":true,"storage_quota":9,"processing_quota":8,"processing_priority":2});
        assert_eq!(client.post(&url).bearer_auth(tokens[0]).json(&update).send().await.unwrap().status(),403);
        assert_eq!(client.post(&url).bearer_auth(tokens[1]).header("x-admin-request","1").json(&update).send().await.unwrap().status(),403);
        let mut invalid = update.clone(); invalid["processing_priority"] = json!(0);
        assert_eq!(client.post(&url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&invalid).send().await.unwrap().status(),400);
        assert_eq!(client.post(&url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&update).send().await.unwrap().status(),200);
        invalid = update.clone(); invalid["is_active"] = json!(false);
        assert_eq!(client.post(format!("{base}/api/admin/users/{admin}")).bearer_auth(tokens[0]).header("x-admin-request","1").json(&invalid).send().await.unwrap().status(),400);
        let whitelist_url = format!("{base}/api/admin/whitelist");
        let ai_url=format!("{base}/api/admin/ta-ai");
        let ai_config=json!({"enabled":true,"format":"AI: {model} / {mode}"});
        assert_eq!(client.get(&ai_url).bearer_auth(tokens[1]).send().await.unwrap().status(),403);
        assert_eq!(client.post(&ai_url).bearer_auth(tokens[0]).json(&ai_config).send().await.unwrap().status(),403);
        assert_eq!(client.post(&ai_url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&json!({"enabled":true,"format":" "})).send().await.unwrap().status(),400);
        assert_eq!(client.post(&ai_url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&ai_config).send().await.unwrap().status(),200);
        let saved:Value=client.get(&ai_url).bearer_auth(tokens[0]).send().await.unwrap().json().await.unwrap();
        assert_eq!(saved,ai_config);
        assert_eq!(client.post(&whitelist_url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&json!({"emails":["invalid"]})).send().await.unwrap().status(),400);
        assert_eq!(client.post(&whitelist_url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&json!({"emails":[" first@example.com ","first@example.com"]})).send().await.unwrap().status(),200);
        // Replacing an existing file must also work on Windows.
        assert_eq!(client.post(&whitelist_url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&json!({"emails":["second@example.com"]})).send().await.unwrap().status(),200);
        assert_eq!(client.post(format!("{url}/revoke-sessions")).bearer_auth(tokens[0]).header("x-admin-request","1").json(&json!({})).send().await.unwrap().status(),200);
        assert!(store.get_session(tokens[1]).await.is_none());
        let restored = FileStore::new(temp.path().into()).await.unwrap();
        assert!(!restored.site_features().await.unwrap().show_reprocess);
        assert!(restored.last_logins().await.unwrap().contains_key(&member.to_string()));
        assert_eq!(restored.load_sidecar::<crate::services::ta_ai::Annotation>(std::path::Path::new(crate::services::ta_ai::ANNOTATION_PATH)).await.unwrap().unwrap().format,"AI: {model} / {mode}");
        let user = restored.get_user(member).await.unwrap();
        assert_eq!((user.storage_quota,user.processing_quota,user.processing_priority),(9,1,2));
        assert_eq!(restored.admin_whitelist().await,vec!["second@example.com"]);
        assert!(restored.get_session(tokens[1]).await.is_none());
        // Credit endpoints use the current identity; grants require both admin and CSRF checks.
        store.insert_session(FileUserSession {session_id:tokens[1],user_id:member,expires_at:Utc::now()+Duration::hours(1),created_at:Utc::now(),user_agent:None,ip_address:None}).await.unwrap();
        let credit_url=format!("{base}/api/note-credits/me");
        assert_eq!(client.get(&credit_url).send().await.unwrap().status(),401);
        let balance=client.get(&credit_url).bearer_auth(tokens[1]).send().await.unwrap();
        assert_eq!(balance.headers()[CACHE_CONTROL],"no-store");
        assert_eq!(balance.json::<Value>().await.unwrap()["balance"],1);
        let grant_url=format!("{url}/note-credits/grant");
        let grant=json!({"amount":10,"reason":"test grant","idempotency_key":"api-retry"});
        assert_eq!(client.post(&grant_url).bearer_auth(tokens[1]).header("x-admin-request","1").json(&grant).send().await.unwrap().status(),403);
        assert_eq!(client.post(&grant_url).bearer_auth(tokens[0]).json(&grant).send().await.unwrap().status(),403);
        for _ in 0..3 {
            let result:Value=client.post(&grant_url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&grant).send().await.unwrap().json().await.unwrap();
            assert_eq!(result["balance_before"],1);assert_eq!(result["balance_after"],11);
        }
        let invalid_grant=json!({"amount":0,"reason":"invalid","idempotency_key":"invalid"});
        assert_eq!(client.post(&grant_url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&invalid_grant).send().await.unwrap().status(),400);
        assert_eq!(client.get(format!("{url}/note-credits/ledger")).bearer_auth(tokens[1]).send().await.unwrap().status(),403);
        let ledger:Value=client.get(format!("{base}/api/note-credits/ledger?user_id={admin}")).bearer_auth(tokens[1]).send().await.unwrap().json().await.unwrap();
        assert_eq!(ledger["entries"].as_array().unwrap().len(),1);
        let global_ledger_url = format!("{base}/api/admin/records?kind=credits&user={member}&status=ADMIN_GRANT");
        assert_eq!(client.get(&global_ledger_url).bearer_auth(tokens[1]).send().await.unwrap().status(), 403);
        let global_ledger: Value = client.get(&global_ledger_url).bearer_auth(tokens[0]).send().await.unwrap().json().await.unwrap();
        assert_eq!(global_ledger["total"], 1);
        let ledger_id = global_ledger["records"][0]["id"].as_str().unwrap();
        assert_eq!(global_ledger["records"][0]["actor_id"], admin.to_string());
        assert!(!global_ledger.to_string().contains("api-retry"));
        let grant_history: Value = client.get(format!("{base}/api/admin/records?kind=activity&q={ledger_id}&status=200"))
            .bearer_auth(tokens[0]).send().await.unwrap().json().await.unwrap();
        assert_eq!(grant_history["total"], 3);
        assert!(grant_history["records"].as_array().unwrap().iter()
            .any(|record| record["detail"]["idempotent_replay"] == false));
        let admin_ledger:Value=client.get(format!("{base}/api/note-credits/ledger?user_id={member}")).bearer_auth(tokens[0]).send().await.unwrap().json().await.unwrap();
        assert!(admin_ledger["entries"].as_array().unwrap().is_empty());
        assert_eq!(client.post(&url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&update).send().await.unwrap().status(),200);
        assert_eq!(store.get_user(member).await.unwrap().processing_quota,11);
        // Disabling a member immediately makes its existing session unusable.
        store.insert_session(FileUserSession { session_id:tokens[1],user_id:member,expires_at:Utc::now()+Duration::hours(1),created_at:Utc::now(),user_agent:None,ip_address:None }).await.unwrap();
        assert_eq!(client.post(&url).bearer_auth(tokens[0]).header("x-admin-request","1").json(&invalid).send().await.unwrap().status(),200);
        assert_eq!(client.get(format!("{base}/api/auth/me")).bearer_auth(tokens[1]).send().await.unwrap().status(),401);
        server.abort();
    }
}
