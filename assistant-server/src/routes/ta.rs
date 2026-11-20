//! Server-rendered TA workspace. New UI and behavior live entirely in Rust.
use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    services::{
        ta::{self, Blackboard, Draft},
    },
};
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Json, Router,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    path::PathBuf,
};

#[path = "ta_view.rs"]
mod view;
#[path = "ta_ai.rs"]
mod ai;

#[derive(Default, Deserialize, Clone)]
pub(super) struct Selection {
    #[serde(default)]
    pub course: String,
    #[serde(default)]
    pub column: String,
    #[serde(default)]
    pub attempt: String,
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub search: String,
    #[serde(default)]
    pub review: String,
    #[serde(default)]
    pub notice: String,
}
impl Selection {
    pub(super) fn url(&self) -> String {
        let mut url = reqwest::Url::parse("http://localhost/ta").unwrap();
        for (key, value) in [
            ("course", &self.course),
            ("column", &self.column),
            ("attempt", &self.attempt),
            ("filter", &self.filter),
            ("search", &self.search),
            ("review", &self.review),
            ("notice", &self.notice),
        ] {
            if !value.is_empty() {
                url.query_pairs_mut().append_pair(key, value);
            }
        }
        format!(
            "{}{}",
            if self.column.is_empty() { "/ta" } else { "/ta/grading" },
            url.query().map(|q| format!("?{q}")).unwrap_or_default()
        )
    }
}
#[derive(Default)]
pub(super) struct Workspace {
    pub ai_job: Option<crate::services::ta_ai::Job>,
    pub courses: Vec<ta::Course>,
    pub assignments: Vec<ta::Assignment>,
    pub queue_cached: bool,
    pub maximum: Option<f64>,
    pub attempts: Vec<ta::Attempt>,
    pub drafts: Vec<Draft>,
    pub submission: Option<ta::Submission>,
    pub error: Option<String>,
}
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/ta/workspace", get(workspace))
        .route("/api/ta/ai/status", get(ai::status))
        .route("/ta/action", post(action))
        .route("/ta/attachment", get(attachment))
}

async fn client(state: &AppState, user: &CurrentUser) -> Result<Blackboard, String> {
    Ok(Blackboard::cached(state, user, false))
}

pub(super) async fn page(_user: CurrentUser) -> Response {
    let Html(html) = super::ui::site_page(&view::shell(), "/ta");
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "same-origin"),
        ],
        Html(html),
    )
        .into_response()
}

async fn workspace(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    Query(mut selection): Query<Selection>,
) -> Response {
    let lock = state.ta_mutation(user.id).await;
    let _guard = lock.lock().await;
    let mut workspace = Workspace::default();
    if let Err(error) = load_workspace(&state, &user, &mut selection, &mut workspace).await {
        tracing::warn!(target: "teaching_ta", error=%error, "加载助教工作台失败");
        workspace.error = Some(error);
    }
    let csrf = csrf_token(&headers, user.id).unwrap_or_default();
    workspace_response(&selection, &workspace, &csrf, StatusCode::OK)
}

fn workspace_response(
    selection: &Selection,
    workspace: &Workspace,
    csrf: &str,
    status: StatusCode,
) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "same-origin"),
        ],
        Json(serde_json::json!({"html":view::fragment(selection, workspace, csrf), "url":selection.url()})),
    )
        .into_response()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkspaceLoad {
    Cached,
    RefreshCourse,
    LoadSubmission,
    EnterAssignment,
}

async fn load_workspace(
    state: &AppState,
    user: &CurrentUser,
    selection: &mut Selection,
    workspace: &mut Workspace,
) -> Result<(), String> {
    let bb = client(state, user).await?;
    load_workspace_with_client(state, user, selection, workspace, &bb, WorkspaceLoad::Cached).await
}

async fn load_workspace_with_client(
    state: &AppState,
    user: &CurrentUser,
    selection: &mut Selection,
    workspace: &mut Workspace,
    bb: &Blackboard,
    load: WorkspaceLoad,
) -> Result<(), String> {
    workspace.courses = bb.courses().await?;
    if selection.course.is_empty() || workspace.courses.is_empty() {
        return Ok(());
    }
    if !workspace.courses.iter().any(|c| c.id == selection.course) {
        return Err("当前账号不是所选课程的助教".into());
    }
    let queue = if load == WorkspaceLoad::RefreshCourse {
        Some(bb.grading_queue(&selection.course).await?)
    } else {
        bb.stored_grading_queue(&selection.course).await?
    };
    workspace.queue_cached = queue.is_some();
    workspace.assignments = queue.unwrap_or_default();
    let pending_columns = state.store.pending_grading_columns(
        &PathBuf::from("ta_drafts").join(user.id.to_string()), &selection.course,
    ).await.map_err(|error| error.to_string())?;
    for column in pending_columns {
        if ta::valid_id(&column).is_ok() && !workspace.assignments.iter().any(|a| a.id == column) {
            workspace.assignments.push(ta::Assignment {
                name: format!("待核对草稿 · {column}"), id: column, attempts: Vec::new(),
            });
        }
    }
    if selection.column.is_empty() {
        return Ok(());
    }
    let Some(assignment) = workspace.assignments.iter_mut().find(|a| a.id == selection.column) else {
        return Ok(());
    };
    workspace.maximum = load_maximum(state, user.id, &selection.course, &selection.column).await?;
    workspace.ai_job = ai::snapshot(state, user.id, &selection.course, &selection.column).await?;
    workspace.drafts = load_drafts(state, user.id, &selection.course, &selection.column).await?;
    workspace.attempts = std::mem::take(&mut assignment.attempts);
    if workspace.attempts.is_empty() {
        selection.review = "1".into();
    }
    if matches!(load, WorkspaceLoad::EnterAssignment | WorkspaceLoad::RefreshCourse)
        && selection.review != "1"
    {
        load_assignment_submissions(bb, &selection.course, workspace).await;
    }
    let visible = visible_attempts(selection, workspace);
    if selection.attempt.is_empty() {
        selection.attempt = visible.first().map(|a| a.id.clone()).unwrap_or_default();
    }
    if selection.review != "1" {
        if let Some(attempt) = visible.iter().find(|a| a.id == selection.attempt) {
            workspace.submission = if load == WorkspaceLoad::LoadSubmission {
                Some(bb.submission(&selection.course, attempt).await?)
            } else {
                bb.stored_submission(&selection.course, attempt).await?
            };
        }
    }
    Ok(())
}

async fn load_assignment_submissions(bb: &Blackboard, course: &str, workspace: &mut Workspace) {
    let mut failed = 0;
    let mut first_error = None;
    for attempt in &workspace.attempts {
        if let Err(error) = bb.submission(course, attempt).await {
            failed += 1;
            if crate::services::pkulogin::is_auth_error(&error)
                || error.contains("尚未绑定教学网")
            {
                workspace.error = Some(error);
                return;
            }
            first_error.get_or_insert(error);
        }
    }
    if let Some(error) = first_error {
        workspace.error = Some(format!("有 {failed} 份提交内容未能加载，可打开对应提交单独重试。{error}"));
    }
}

pub(super) fn visible_attempts<'a>(
    selection: &Selection,
    workspace: &'a Workspace,
) -> Vec<&'a ta::Attempt> {
    let search = selection.search.to_lowercase();
    workspace.attempts.iter()
        .filter(|a| a.student_name.to_lowercase().contains(&search))
        .filter(|a| selection.filter != "draft" || workspace.drafts.iter()
            .any(|d| !d.synced && d.attempt_id == a.id))
        .collect()
}

fn maximum_path(user: uuid::Uuid, course: &str, column: &str) -> Result<PathBuf, String> {
    ta::valid_id(course)?;
    ta::valid_id(column)?;
    Ok(PathBuf::from("ta_maximum").join(user.to_string()).join(format!("{course}.{column}.json")))
}

async fn load_maximum(state: &AppState, user: uuid::Uuid, course: &str, column: &str) -> Result<Option<f64>, String> {
    state.store.load_sidecar(&maximum_path(user, course, column)?).await
}

async fn require_maximum(state: &AppState, user: uuid::Uuid, course: &str, column: &str) -> Result<f64, String> {
    load_maximum(state, user, course, column).await?.ok_or("请先在严小希设置本作业满分".into())
}

async fn save_maximum(state: &AppState, user: uuid::Uuid, course: &str, column: &str, maximum: f64) -> Result<(), String> {
    if !maximum.is_finite() || maximum <= 0.0 {
        return Err("满分必须大于 0".into());
    }
    ta::validate_score(maximum, maximum)?;
    state.store.save_sidecar(&maximum_path(user, course, column)?, &maximum.to_string()).await
}

fn draft_path(user: uuid::Uuid, course: &str, column: &str) -> Result<PathBuf, String> {
    ta::valid_id(course)?;
    ta::valid_id(column)?;
    Ok(PathBuf::from("ta_drafts")
        .join(user.to_string())
        .join(format!("{course}.{column}.json")))
}
async fn load_drafts(
    state: &AppState,
    user: uuid::Uuid,
    course: &str,
    column: &str,
) -> Result<Vec<Draft>, String> {
    state.store.grading_drafts(&draft_path(user, course, column)?).await.map_err(|error| error.to_string())
}
async fn persist(
    state: &AppState,
    user: uuid::Uuid,
    course: &str,
    column: &str,
    drafts: &[Draft],
) -> Result<(), String> {
    state.store.save_grading_drafts(&draft_path(user, course, column)?, drafts, true)
        .await.map(|_| ()).map_err(|error| error.to_string())
}

fn csrf_token(headers: &HeaderMap, user: uuid::Uuid) -> Option<String> {
    let token = crate::auth::extract_session_token(headers)?;
    Some(format!(
        "{:x}",
        Sha256::digest(format!("ta-action-v1:{user}:{token}"))
    ))
}
fn field<'a>(form: &'a [(String, String)], key: &str) -> &'a str {
    form.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}
fn selection_from(form: &[(String, String)]) -> Selection {
    Selection {
        course: field(form, "course").into(),
        column: field(form, "column").into(),
        attempt: field(form, "attempt").into(),
        filter: field(form, "filter").into(),
        search: field(form, "search").into(),
        review: field(form, "review").into(),
        ..Default::default()
    }
}

async fn action(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    audit: Option<axum::Extension<super::operations::AuditId>>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let expected = csrf_token(&headers, user.id);
    if expected.as_deref() != Some(field(&form, "csrf")) || expected.is_none() {
        return ApiError::Forbidden("页面凭据已失效，请刷新后再操作".into()).into_response();
    }
    let mut selection = selection_from(&form);
    if matches!(field(&form, "action"), "commit" | "discard") {
        selection.review = "1".into();
    }
    if let Some(axum::Extension(id)) = audit {
        let operation = match field(&form, "action") {
            "ai_start" => "ta.ai_start",
            "ai_cancel" => "ta.ai_cancel",
            "commit" => "ta.commit",
            "discard" => "ta.discard",
            "save_bulk" | "full_bulk" => "ta.save_bulk",
            "save_single" | "save_single_next" => "ta.save_single",
            _ => "ta.action",
        };
        let detail = serde_json::json!({
            "course": ta::valid_id(&selection.course).ok(),
            "column": ta::valid_id(&selection.column).ok(),
            "attempt": ta::valid_id(&selection.attempt).ok(),
        });
        if let Err(error) = state.store.describe_audit(id.0, operation, &detail).await {
            tracing::error!(%error, audit_id=id.0, "助教操作目标登记失败");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    let lock = state.ta_mutation(user.id).await;
    let _guard = lock.lock().await;
    let operation = field(&form, "action");
    if matches!(operation, "refresh_courses" | "refresh" | "load_submission" | "enter_assignment") {
        let load = match operation {
            "refresh" => WorkspaceLoad::RefreshCourse,
            "load_submission" => WorkspaceLoad::LoadSubmission,
            "enter_assignment" => WorkspaceLoad::EnterAssignment,
            _ => WorkspaceLoad::Cached,
        };
        let mut workspace = Workspace::default();
        let bb = if load == WorkspaceLoad::RefreshCourse {
            Blackboard::live_checks(&state, &user)
        } else {
            Blackboard::cached(&state, &user, false)
        };
        if operation == "refresh_courses" {
            match bb.refresh_courses().await {
                Ok(courses) => {
                    if !courses.iter().any(|course| course.id == selection.course) {
                        selection = Selection::default();
                    }
                    selection.notice = format!("已从教学网刷新助教课程，共 {} 门", courses.len());
                }
                Err(error) => workspace.error = Some(format!("刷新课程失败，已有本地缓存保留：{error}")),
            }
        }
        match load_workspace_with_client(&state, &user, &mut selection, &mut workspace, &bb, load).await {
            Ok(()) if operation == "refresh" && workspace.error.is_none() => {
                selection.notice = "已从教学网刷新当前课程的待评分队列".into();
            }
            Err(error) if load == WorkspaceLoad::RefreshCourse => {
                workspace = Workspace::default();
                let _ = load_workspace(&state, &user, &mut selection, &mut workspace).await;
                workspace.error = Some(format!("刷新课程作业失败：{error}"));
            }
            Err(error) if workspace.error.is_none() => workspace.error = Some(error),
            _ => {}
        }
        if headers.get("x-ta-partial").is_some_and(|v| v == "1") {
            return workspace_response(&selection, &workspace, expected.as_deref().unwrap_or(""), StatusCode::OK);
        }
        let Html(html) = super::ui::site_page(&view::render(&selection, &workspace, expected.as_deref().unwrap_or("")), "/ta");
        return ([(header::CACHE_CONTROL, "no-store")], Html(html)).into_response();
    }
    let result = mutate(&state, &user, &form, &mut selection).await;
    if matches!(field(&form,"action"), "ai_start" | "ai_cancel") && headers.get("x-ta-partial").is_some_and(|v|v=="1") {
        return match result {
            Ok(message) => {
                let job=ai::snapshot(&state,user.id,&selection.course,&selection.column).await.ok().flatten();
                ([(header::CACHE_CONTROL,"no-store")],Json(serde_json::json!({"ai":true,"html":view::ai_status(job.as_ref(),&selection),"active":job.as_ref().is_some_and(|j|j.active()),"message":message}))).into_response()
            },
            Err(error)=>(StatusCode::BAD_REQUEST,[(header::CACHE_CONTROL,"no-store")],Json(serde_json::json!({"error":error}))).into_response()
        };
    }
    match result {
        Ok(message) => {
            selection.notice = message;
            if headers.get("x-ta-partial").is_some_and(|v| v == "1") {
                let mut workspace = Workspace::default();
                if let Err(error) =
                    load_workspace(&state, &user, &mut selection, &mut workspace).await
                {
                    workspace.error = Some(format!("操作已完成，但刷新数据失败：{error}"));
                }
                workspace_response(
                    &selection,
                    &workspace,
                    expected.as_deref().unwrap_or(""),
                    StatusCode::OK,
                )
            } else {
                Redirect::to(&selection.url()).into_response()
            }
        }
        Err(error) => {
            // Preserve editable inputs on failures, and never put feedback/credentials in the URL.
            let mut workspace = Workspace::default();
            let _ = load_workspace(&state, &user, &mut selection, &mut workspace).await;
            let score = field(&form, "score").parse::<f64>();
            if field(&form, "action").starts_with("save_single") {
                if let (Ok(score), Some(attempt)) = (
                    score,
                    workspace
                        .attempts
                        .iter()
                        .find(|a| a.id == selection.attempt),
                ) {
                    workspace.drafts.retain(|d| d.attempt_id != attempt.id);
                    workspace.drafts.push(Draft {
                        attempt_id: attempt.id.clone(),
                        student_name: attempt.student_name.clone(),
                        score,
                        feedback: field(&form, "feedback").into(),
                        revision: attempt.revision.clone(),
                        updated_at: String::new(),
                        synced: false,
                        error: None,
                    });
                }
            }
            workspace.error = Some(error);
            if headers.get("x-ta-partial").is_some_and(|v| v == "1") {
                return workspace_response(
                    &selection,
                    &workspace,
                    expected.as_deref().unwrap_or(""),
                    StatusCode::BAD_REQUEST,
                );
            }
            let Html(html) = super::ui::site_page(
                &view::render(&selection, &workspace, expected.as_deref().unwrap_or("")),
                "/ta",
            );
            (
                StatusCode::BAD_REQUEST,
                [(header::CACHE_CONTROL, "no-store")],
                Html(html),
            )
                .into_response()
        }
    }
}

async fn mutate(
    state: &AppState,
    user: &CurrentUser,
    form: &[(String, String)],
    selection: &mut Selection,
) -> Result<String, String> {
    match field(form,"action") {
        "ai_start" => return ai::start(state,user,form,selection).await,
        "ai_cancel" => return ai::cancel(state,user,form,selection).await,
        _ => {}
    }
    // Publishing grades requires a fresh view of the pending queue.
    let bb = Blackboard::cached(state, user, field(form, "action") == "commit");
    mutate_with_client(state, user, form, selection, &bb).await
}

async fn mutate_with_client(
    state: &AppState,
    user: &CurrentUser,
    form: &[(String, String)],
    selection: &mut Selection,
    bb: &Blackboard,
) -> Result<String, String> {
    bb.authorize(&selection.course).await?;
    let action = field(form, "action");
    if action == "discard" {
        let selected: HashSet<_> = form
            .iter()
            .filter(|(k, _)| k == "draft")
            .map(|(_, v)| v.as_str())
            .collect();
        if selected.is_empty() {
            return Err("请先选择要丢弃的草稿".into());
        }
        state.store.discard_grading_drafts(&draft_path(user.id, &selection.course, &selection.column)?, &selected)
            .await.map_err(|error| error.to_string())?;
        selection.review = "1".into();
        return Ok("已丢弃选中的本地草稿".into());
    }
    if action == "commit" && field(form, "confirmed") != "yes" {
        return Err("请勾选确认，将选中草稿的分数和评语写入教学网".into());
    }
    let attempts = bb.grading_queue(&selection.course).await?.into_iter()
        .find(|a| a.id == selection.column).ok_or("该作业当前没有待评分提交")?.attempts;
    if action == "save_maximum" {
        let maximum = field(form, "maximum").parse().map_err(|_| "请输入有效的作业满分")?;
        save_maximum(state, user.id, &selection.course, &selection.column, maximum).await?;
        return Ok(format!("已将本作业的本地满分设为 {maximum}"));
    }
    let maximum = require_maximum(state, user.id, &selection.course, &selection.column).await?;
    let mut drafts = load_drafts(state, user.id, &selection.course, &selection.column).await?;
    if action == "commit" {
        let selected: HashSet<_> = form
            .iter()
            .filter(|(k, _)| k == "draft")
            .map(|(_, v)| v.as_str())
            .collect();
        let indices: Vec<_> = drafts
            .iter()
            .enumerate()
            .filter(|(_, d)| !d.synced && selected.contains(d.attempt_id.as_str()))
            .map(|(i, _)| i)
            .collect();
        if indices.is_empty() || indices.len() > 200 {
            return Err("请选择 1 到 200 份待同步草稿".into());
        }
        // Validate the entire set before the first remote write.
        for &i in &indices {
            let d = &drafts[i];
            ta::validate_score(d.score, maximum)?;
            let a = attempts
                .iter()
                .find(|a| a.id == d.attempt_id)
                .ok_or("提交记录已变化，请重新批改")?;
            if a.revision != d.revision {
                return Err("教学网提交或评分状态已变化，请丢弃旧草稿并重新核对".into());
            }
            if d.error.is_some() {
                return Err(
                    "存在结果未确认的登分记录，请先到教学网核对，再丢弃草稿或重新批改".into(),
                );
            }
        }
        let mut done = 0;
        for i in indices {
            // Recheck immediately before each write, including changes during a long batch.
            let fresh = bb.grading_queue(&selection.course).await?;
            let fresh = fresh.iter().find(|a| a.id == selection.column)
                .map(|a| a.attempts.as_slice()).unwrap_or_default();
            let d = &drafts[i];
            let a = fresh
                .iter()
                .find(|a| a.id == d.attempt_id)
                .ok_or_else(|| format!("已同步 {done} 份；下一份不在待评分队列中"))?;
            if a.revision != d.revision {
                return Err(format!(
                    "已同步 {done} 份；下一份状态发生变化，已停止，请刷新核对"
                ));
            }
            // Persist intent before sending: a crash must not lead to an unreviewed replay.
            drafts[i].error = Some("登分已开始，结果尚未确认；请先核对教学网".into());
            persist(
                state,
                user.id,
                &selection.course,
                &selection.column,
                std::slice::from_ref(&drafts[i]),
            )
            .await?;
            let d = &drafts[i];
            match bb
                .save_grade(
                    &selection.course,
                    &selection.column,
                    a,
                    d.score,
                    &d.feedback,
                )
                .await
            {
                Ok(()) => {
                    drafts[i].synced = true;
                    drafts[i].error = None;
                    done += 1;
                }
                Err(error) => {
                    drafts[i].error = Some(error.clone());
                    persist(
                        state,
                        user.id,
                        &selection.course,
                        &selection.column,
                        std::slice::from_ref(&drafts[i]),
                    )
                    .await?;
                    selection.review = "1".into();
                    return Err(format!("已同步 {done} 份，其余已停止：{error}"));
                }
            }
            persist(
                state,
                user.id,
                &selection.course,
                &selection.column,
                std::slice::from_ref(&drafts[i]),
            )
            .await?;
        }
        if let Err(error) = bb.grading_queue(&selection.course).await {
            tracing::warn!(target: "teaching_ta", %error, "登分完成后未能刷新待评分队列");
        }
        selection.review = "1".into();
        return Ok(format!("已同步并回读确认 {done} 份评分"));
    }
    if !matches!(
        action,
        "save_bulk" | "full_bulk" | "save_single" | "save_single_next"
    ) {
        return Err("无法识别此操作".into());
    }
    let score = ta::validate_score(
        if action == "full_bulk" {
            maximum
        } else {
            field(form, "score").parse().map_err(|_| "请输入有效分数")?
        },
        maximum,
    )?;
    let feedback = field(form, "feedback");
    if feedback.chars().count() > 10_000 {
        return Err("评语不能超过 10000 字".into());
    }
    let selected: Vec<(&str, &str)> = if matches!(action, "save_bulk" | "full_bulk") {
        let key = if field(form, "select_visible") == "yes" {
            "visible"
        } else {
            "selected"
        };
        form.iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.split_once(':').ok_or("提交选择无效".to_string()))
            .collect::<Result<_, _>>()?
    } else {
        vec![(field(form, "attempt"), field(form, "revision"))]
    };
    let unique: HashSet<_> = selected.iter().map(|(id, _)| *id).collect();
    if unique.is_empty() || unique.len() != selected.len() || selected.len() > 200 {
        return Err("请选择 1 到 200 份不同的提交".into());
    }
    for &(id, revision) in &selected {
        let a = attempts
            .iter()
            .find(|a| a.id == id)
            .ok_or("所选提交不存在")?;
        if a.revision != revision {
            return Err("提交或成绩已变化，请刷新页面后重新核对".into());
        }
        if drafts
            .iter()
            .any(|d| d.attempt_id == id && !d.synced && d.error.is_some())
        {
            return Err("这份提交有结果未确认的登分，请先核对教学网并丢弃旧草稿".into());
        }
    }
    let mut edits = Vec::with_capacity(selected.len());
    for &(id, _) in &selected {
        let a = attempts.iter().find(|a| a.id == id).unwrap();
        edits.push(Draft {
            attempt_id: id.into(),
            student_name: a.student_name.clone(),
            score,
            feedback: feedback.into(),
            revision: a.revision.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            synced: false,
            error: None,
        });
    }
    persist(state, user.id, &selection.course, &selection.column, &edits)
    .await?;
    if action == "save_single_next" {
        let next = field(form, "next");
        if attempts.iter().any(|a| a.id == next) {
            selection.attempt = next.into();
        }
    }
    Ok(format!(
        "已保存 {} 份评分草稿，尚未写入教学网",
        selected.len()
    ))
}

#[derive(Deserialize)]
struct Download {
    course: String,
    column: String,
    attempt: String,
    file: String,
    #[serde(default)]
    preview: bool,
}
async fn attachment(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(query): Query<Download>,
) -> Result<Response, ApiError> {
    let lock = state.ta_mutation(user.id).await;
    let _guard = lock.lock().await;
    let bb = client(&state, &user).await.map_err(ApiError::BadRequest)?;
    bb.authorize(&query.course)
        .await
        .map_err(ApiError::Forbidden)?;
    let queue = bb.grading_queue(&query.course).await.map_err(ApiError::BadRequest)?;
    let attempt = queue.iter().find(|a| a.id == query.column)
        .and_then(|a| a.attempts.iter().find(|a| a.id == query.attempt))
        .ok_or(ApiError::NotFound)?;
    let submission = bb.submission(&query.course, attempt).await.map_err(ApiError::BadRequest)?;
    let file = submission
        .files
        .iter()
        .find(|f| f.id == query.file)
        .ok_or(ApiError::NotFound)?;
    let bytes = bb.download(file).await.map_err(ApiError::BadRequest)?;
    let pdf = file.name.to_ascii_lowercase().ends_with(".pdf") && bytes.starts_with(b"%PDF-");
    let disposition = if query.preview && pdf {
        "inline"
    } else {
        "attachment"
    };
    let filename = format!("{}_{}", attempt.student_name, file.name);
    let disposition = format!(
        "{disposition}; filename=\"submission\"; filename*=UTF-8''{}",
        urlencoding::encode(&filename)
    );
    Ok((
        [
            (
                header::CONTENT_TYPE,
                if pdf {
                    "application/pdf".into()
                } else {
                    "application/octet-stream".into()
                },
            ),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, "no-store".into()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
            // CSP sandbox blocks Chromium's built-in PDF viewer. Restrict framing while
            // keeping PDFs viewable; every non-PDF remains an octet-stream attachment.
            (
                header::CONTENT_SECURITY_POLICY,
                "frame-ancestors 'self'".into(),
            ),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn csrf_is_bound_to_login_and_feedback_is_never_in_redirect() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "assistant_token=first".parse().unwrap());
        let id = uuid::Uuid::new_v4();
        let first = csrf_token(&h, id).unwrap();
        h.insert(header::COOKIE, "assistant_token=second".parse().unwrap());
        assert_ne!(first, csrf_token(&h, id).unwrap());
        let s = selection_from(&[
            ("course".into(), "_1_1".into()),
            ("feedback".into(), "private".into()),
        ]);
        assert!(!s.url().contains("private"));
        assert!(draft_path(id, "../", "_1_1").is_err());
    }
    #[tokio::test]
    async fn real_urlencoded_bulk_form_preserves_multiple_selections() {
        let app = Router::new().route(
            "/decode",
            post(|Form(pairs): Form<Vec<(String, String)>>| async move { axum::Json(pairs) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/decode", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(url)
            .form(&[
                ("selected", "_1:a"),
                ("selected", "_2:b"),
                ("feedback", "中文 & 评语"),
            ])
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let pairs: Vec<(String, String)> = response.json().await.unwrap();
        assert_eq!(pairs.iter().filter(|(key, _)| key == "selected").count(), 2);
        assert_eq!(field(&pairs, "feedback"), "中文 & 评语");
        server.abort();
    }

    #[tokio::test]
    async fn grading_saves_locally_then_commits_only_the_selected_attempt_and_blocks_uncertain_retries(
    ) {
        use axum::{routing::get, Json};
        use serde_json::json;
        use std::{collections::HashMap, sync::{Arc, Mutex}};
        #[derive(Default)]
        struct Remote {
            score: Option<f64>,
            writes: Vec<(String, String)>,
            uncertain: bool,
            submission_reads: Vec<String>,
        }
        let remote = Arc::new(Mutex::new(Remote::default()));
        let app = Router::new()
            .route("/webapps/portal/execute/tabs/tabAction", get(|| async {
                axum::response::Html(r#"<a href=" /webapps/blackboard/execute/launcher?type=Course&amp;id=PkId{key=_c_1, dataType=blackboard.data.course.Course}&amp;url=">编号: 测试课程</a>"#)
            }))
            .route("/webapps/gradebook/do/instructor/viewNeedsGrading", get(|State(r): State<Arc<Mutex<Remote>>>| async move {
                let mut rows = ta::grading_row("_c_1", "_h_1", "作业", "_a_old", "2026年11月10日");
                if r.lock().unwrap().score.is_none() {
                    rows.push_str(&ta::grading_row("_c_1", "_h_1", "作业", "_a_1", "2026年11月12日"));
                }
                Html(format!("<table>{rows}</table>"))
            }))
            .route("/webapps/assignment/gradeAssignmentRedirector", get(|State(r): State<Arc<Mutex<Remote>>>, Query(query): Query<HashMap<String, String>>| async move {
                r.lock().unwrap().submission_reads.push(query["attempt_id"].clone());
                Html("<div id='submissionText'>待评分正文</div>")
            }))
            .route("/webapps/gradebook/controller/reconcileGrades",get(||async {Html("<input name='blackboard.platform.security.NonceUtil.nonce.ajax' value='nonce'>")}))
            .route("/webapps/gradebook/controller/saveReconcileGrade",post(|State(r):State<Arc<Mutex<Remote>>>,Form(f):Form<HashMap<String,String>>|async move {
                assert_eq!(f["blackboard.platform.security.NonceUtil.nonce.ajax"],"nonce");
                let mut r=r.lock().unwrap();r.writes.push((f["attemptId"].clone(),f["myfeedbacktext"].clone()));
                if !r.uncertain {r.score=Some(f["score"].parse().unwrap());}
                Json(json!({"success":true}))
            }))
            .route("/webapps/gradebook/controller/loadReconcileData",get(|State(r):State<Arc<Mutex<Remote>>>|async move {
                Json(json!({"attempts":[{"attemptId":"_a_1","reconciledScore":r.lock().unwrap().score}]}))
            })).with_state(remote.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bb = Blackboard::test_client(&format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let tmp = tempdir::TempDir::new("ta-grading-test").unwrap();
        let settings:crate::config::Settings=serde_json::from_value(json!({
            "server":{"address":"127.0.0.1","port":0},
            "wechat":{"api_base_url":"","oauth_app_id":"","oauth_app_secret":"","redirect_uri":"","qr_bot_type":""},
            "security":{"session_ttl_hours":1},"cors":{"allowed_origins":[],"allow_credentials":false},
            "audioapi_paraformer":{},"llm_deepseek":{}
        })).unwrap();
        let state = AppState::new(
            settings,
            Arc::new(
                crate::store::FileStore::new(tmp.path().into())
                    .await
                    .unwrap(),
            ),
        );
        let user = CurrentUser {
            id: uuid::Uuid::new_v4(),
            username: "助教".into(),
            email: "test@pku.edu.cn".into(),
        };
        let mut s = Selection {
            course: "_c_1".into(),
            column: "_h_1".into(),
            ..Default::default()
        };
        let invalid_max = vec![("action".into(), "save_maximum".into()), ("maximum".into(), "99.999".into())];
        assert!(mutate_with_client(&state, &user, &invalid_max, &mut s, &bb).await.is_err());
        let max = vec![("action".into(), "save_maximum".into()), ("maximum".into(), "100".into())];
        mutate_with_client(&state, &user, &max, &mut s, &bb).await.unwrap();
        assert_eq!(load_maximum(&state, user.id, &s.course, &s.column).await.unwrap(), Some(100.0));
        assert_eq!(load_maximum(&state, uuid::Uuid::new_v4(), &s.course, &s.column).await.unwrap(), None);
        let queue = bb.grading_queue(&s.course).await.unwrap();
        let attempts = &queue[0].attempts;
        let mut workspace = Workspace { attempts: attempts.clone(), ..Default::default() };
        load_assignment_submissions(&bb, &s.course, &mut workspace).await;
        assert!(workspace.error.is_none());
        assert_eq!(remote.lock().unwrap().submission_reads, ["_a_old", "_a_1"]);
        let revision = attempts
            .iter()
            .find(|a| a.id == "_a_1")
            .unwrap()
            .revision
            .clone();
        let save = vec![
            ("action".into(), "save_bulk".into()),
            ("selected".into(), format!("_a_1:{revision}")),
            ("score".into(), "89.5".into()),
            ("feedback".into(), "请补充复杂度分析".into()),
        ];
        mutate_with_client(&state, &user, &save, &mut s, &bb)
            .await
            .unwrap();
        assert!(remote.lock().unwrap().writes.is_empty());
        assert_eq!(
            load_drafts(&state, user.id, &s.course, &s.column)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            load_drafts(&state, uuid::Uuid::new_v4(), &s.course, &s.column)
                .await
                .unwrap()
                .is_empty()
        );
        let mut commit = vec![
            ("action".into(), "commit".into()),
            ("draft".into(), "_a_1".into()),
        ];
        assert!(mutate_with_client(&state, &user, &commit, &mut s, &bb)
            .await
            .is_err());
        assert!(remote.lock().unwrap().writes.is_empty());
        commit.push(("confirmed".into(), "yes".into()));
        let draft_directory = PathBuf::from("ta_drafts").join(user.id.to_string());
        assert_eq!(
            state.store.pending_grading_columns(&draft_directory, &s.course).await.unwrap(),
            [s.column.clone()],
        );
        // An external change invalidates the draft before any write.
        remote.lock().unwrap().score = Some(1.0);
        assert!(mutate_with_client(&state, &user, &commit, &mut s, &bb)
            .await
            .is_err());
        assert!(remote.lock().unwrap().writes.is_empty());
        remote.lock().unwrap().score = None;
        mutate_with_client(&state, &user, &commit, &mut s, &bb)
            .await
            .unwrap();
        assert_eq!(
            remote.lock().unwrap().writes,
            vec![("_a_1".into(), "请补充复杂度分析".into())]
        );
        assert!(
            load_drafts(&state, user.id, &s.course, &s.column)
                .await
                .unwrap()[0]
                .synced
        );
        assert!(state.store.pending_grading_columns(&draft_directory, &s.course).await.unwrap().is_empty());
        // A second click must never write a synced draft again.
        assert!(mutate_with_client(&state, &user, &commit, &mut s, &bb)
            .await
            .is_err());
        assert_eq!(remote.lock().unwrap().writes.len(), 1);
        // An unconfirmed HTTP-200 write is retained and cannot be blindly retried.
        remote.lock().unwrap().score = None;
        remote.lock().unwrap().uncertain = true;
        mutate_with_client(&state, &user, &save, &mut s, &bb)
            .await
            .unwrap();
        assert!(mutate_with_client(&state, &user, &commit, &mut s, &bb)
            .await
            .is_err());
        let stored = load_drafts(&state, user.id, &s.course, &s.column)
            .await
            .unwrap();
        assert!(!stored[0].synced && stored[0].error.is_some());
        assert!(mutate_with_client(&state, &user, &commit, &mut s, &bb)
            .await
            .is_err());
        assert_eq!(remote.lock().unwrap().writes.len(), 2);
        server.abort();
    }
}
