//! Server-rendered TA workspace. New UI and behavior live entirely in Rust.
use crate::{
    app_state::AppState,
    auth::CurrentUser,
    error::ApiError,
    services::{
        pkulogin,
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
    collections::{HashMap, HashSet},
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
    pub group: String,
    #[serde(default)]
    pub attempt: String,
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub search: String,
    #[serde(default)]
    pub history: String,
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
            ("group", &self.group),
            ("attempt", &self.attempt),
            ("filter", &self.filter),
            ("search", &self.search),
            ("history", &self.history),
            ("review", &self.review),
            ("notice", &self.notice),
        ] {
            if !value.is_empty() {
                url.query_pairs_mut().append_pair(key, value);
            }
        }
        format!(
            "/ta{}",
            url.query().map(|q| format!("?{q}")).unwrap_or_default()
        )
    }
}
#[derive(Default)]
pub(super) struct Workspace {
    pub ai_job: Option<crate::services::ta_ai::Job>,
    pub courses: Vec<ta::Course>,
    pub assignments: Vec<ta::Assignment>,
    pub groups: Vec<ta::Group>,
    pub students: HashMap<String, ta::Student>,
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
    let username = user
        .email
        .split('@')
        .next()
        .ok_or("无法识别当前教学网账号")?;
    // A live lease takes precedence; no new credentials or login component are introduced.
    if let Some(login) = state.login_cache.get(username).await {
        return Blackboard::from_session(login);
    }
    let stored = state.store.get_user(user.id).await.ok_or("用户不存在")?;
    let password = stored
        .iaaa_encrypted_password
        .ok_or("尚未绑定教学网，请前往用户中心完成现有 IAAA 绑定")?;
    let login =
        pkulogin::login_iaaa_and_get_cookies_with_cache(username, &password, &state.login_cache)
            .await?;
    Blackboard::from_session(login)
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

async fn load_workspace(
    state: &AppState,
    user: &CurrentUser,
    selection: &mut Selection,
    workspace: &mut Workspace,
) -> Result<(), String> {
    let bb = client(state, user).await?;
    workspace.courses = bb.courses().await?;
    if selection.course.is_empty() {
        selection.course = workspace
            .courses
            .first()
            .map(|c| c.id.clone())
            .unwrap_or_default();
    }
    if selection.course.is_empty() {
        return Ok(());
    }
    if !workspace.courses.iter().any(|c| c.id == selection.course) {
        return Err("当前账号不是所选课程的助教".into());
    }
    workspace.assignments = bb.assignments(&selection.course).await?;
    workspace.groups = bb.groups(&selection.course).await?;
    if selection.column.is_empty() {
        selection.column = workspace
            .assignments
            .first()
            .map(|a| a.id.clone())
            .unwrap_or_default();
    }
    if selection.column.is_empty() {
        return Ok(());
    }
    if !workspace
        .assignments
        .iter()
        .any(|a| a.id == selection.column)
    {
        return Err("未找到所选作业".into());
    }
    workspace.ai_job = ai::snapshot(state, user.id, &selection.course, &selection.column).await?;
    workspace.students = bb.students(&selection.course).await?;
    workspace.attempts = bb.attempts(&selection.course, &selection.column).await?;
    workspace.drafts = load_drafts(state, user.id, &selection.course, &selection.column).await?;
    if !selection.group.is_empty() {
        if !workspace.groups.iter().any(|g| g.id == selection.group) {
            return Err("批改组不存在".into());
        }
        let members = bb.group_users(&selection.course, &selection.group).await?;
        workspace.students.retain(|id, _| members.contains(id));
    }
    workspace
        .attempts
        .retain(|a| workspace.students.contains_key(&a.user_id));
    workspace.attempts.sort_by(|a, b| {
        let sa = &workspace.students[&a.user_id];
        let sb = &workspace.students[&b.user_id];
        sa.number
            .cmp(&sb.number)
            .then(b.date.cmp(&a.date))
            .then(b.id.cmp(&a.id))
    });
    let visible = visible_attempts(selection, workspace);
    if selection.attempt.is_empty() {
        selection.attempt = visible.first().map(|a| a.id.clone()).unwrap_or_default();
    }
    if let Some(attempt) = visible
        .iter()
        .find(|a| selection.review != "1" && a.id == selection.attempt)
    {
        workspace.submission = Some(
            bb.submission(
                &selection.course,
                &selection.column,
                attempt,
                &workspace.students[&attempt.user_id],
            )
            .await?,
        );
    }
    Ok(())
}

pub(super) fn visible_attempts<'a>(
    selection: &Selection,
    workspace: &'a Workspace,
) -> Vec<&'a ta::Attempt> {
    let latest = ta::latest_attempts(&workspace.attempts);
    let search = selection.search.to_lowercase();
    workspace
        .attempts
        .iter()
        .filter(|a| {
            let student = &workspace.students[&a.user_id];
            let pending = workspace
                .drafts
                .iter()
                .any(|d| d.attempt_id == a.id && !d.synced);
            (selection.history == "1" || latest.contains(&a.id))
                && (search.is_empty()
                    || format!("{} {}", student.name, student.number)
                        .to_lowercase()
                        .contains(&search))
                && match selection.filter.as_str() {
                    "ungraded" => !is_graded(a) && !a.exempt,
                    "graded" => is_graded(a),
                    "draft" => pending,
                    _ => true,
                }
        })
        .collect()
}
pub(super) fn is_graded(attempt: &ta::Attempt) -> bool {
    ta::attempt_is_graded(&attempt.status, attempt.score)
}
pub(super) fn can_grade(attempt: &ta::Attempt) -> bool {
    !attempt.exempt && !matches!(attempt.status.to_ascii_lowercase().as_str(), "inprogress" | "inprogressagain")
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
    Ok(state
        .store
        .load_sidecar::<Vec<Draft>>(&draft_path(user, course, column)?)
        .await?
        .unwrap_or_default())
}
async fn persist(
    state: &AppState,
    user: uuid::Uuid,
    course: &str,
    column: &str,
    drafts: &[Draft],
) -> Result<(), String> {
    let payload = serde_json::to_string(drafts).map_err(|_| "无法保存评分草稿")?;
    state
        .store
        .save_sidecar(&draft_path(user, course, column)?, &payload)
        .await
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
        group: field(form, "group").into(),
        attempt: field(form, "attempt").into(),
        filter: field(form, "filter").into(),
        search: field(form, "search").into(),
        history: field(form, "history").into(),
        ..Default::default()
    }
}

async fn action(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
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
    let lock = state.ta_mutation(user.id).await;
    let _guard = lock.lock().await;
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
                        student_id: attempt.user_id.clone(),
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
    let bb = client(state, user).await?;
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
    let assignment = bb.assignment(&selection.course, &selection.column).await?;
    let mut drafts = load_drafts(state, user.id, &selection.course, &selection.column).await?;
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
        drafts.retain(|d| !selected.contains(d.attempt_id.as_str()) || d.synced);
        persist(
            state,
            user.id,
            &selection.course,
            &selection.column,
            &drafts,
        )
        .await?;
        selection.review = "1".into();
        return Ok("已丢弃选中的本地草稿".into());
    }
    let attempts = bb.attempts(&selection.course, &selection.column).await?;
    if action == "commit" {
        if field(form, "confirmed") != "yes" {
            return Err("请勾选确认，将选中草稿的分数和评语写入教学网".into());
        }
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
            ta::validate_score(d.score, assignment.possible)?;
            let a = attempts
                .iter()
                .find(|a| a.id == d.attempt_id && a.user_id == d.student_id)
                .ok_or("提交记录已变化，请重新批改")?;
            if !can_grade(a) || a.revision != d.revision {
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
            let fresh = bb.attempts(&selection.course, &selection.column).await?;
            let d = &drafts[i];
            let a = fresh
                .iter()
                .find(|a| a.id == d.attempt_id && a.user_id == d.student_id)
                .ok_or("提交记录已不存在")?;
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
                &drafts,
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
                        &drafts,
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
                &drafts,
            )
            .await?;
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
            assignment.possible
        } else {
            field(form, "score").parse().map_err(|_| "请输入有效分数")?
        },
        assignment.possible,
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
    let members = if selection.group.is_empty() {
        None
    } else {
        Some(bb.group_users(&selection.course, &selection.group).await?)
    };
    for &(id, revision) in &selected {
        let a = attempts
            .iter()
            .find(|a| a.id == id)
            .ok_or("所选提交不存在")?;
        if !can_grade(a) {
            return Err("所选提交已豁免或仍在提交中，不能评分".into());
        }
        if members
            .as_ref()
            .is_some_and(|members| !members.contains(&a.user_id))
        {
            return Err("所选提交不属于当前批改组".into());
        }
        if a.revision != revision {
            return Err("提交或成绩已变化，请刷新页面后重新核对".into());
        }
        if is_graded(a) && field(form, "overwrite") != "yes" {
            return Err("包含已评分提交，请明确勾选允许修改已评分记录".into());
        }
        if drafts
            .iter()
            .any(|d| d.attempt_id == id && !d.synced && d.error.is_some())
        {
            return Err("这份提交有结果未确认的登分，请先核对教学网并丢弃旧草稿".into());
        }
    }
    for &(id, _) in &selected {
        let a = attempts.iter().find(|a| a.id == id).unwrap();
        drafts.retain(|d| d.attempt_id != id);
        drafts.push(Draft {
            attempt_id: id.into(),
            student_id: a.user_id.clone(),
            score,
            feedback: feedback.into(),
            revision: a.revision.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            synced: false,
            error: None,
        });
    }
    persist(
        state,
        user.id,
        &selection.course,
        &selection.column,
        &drafts,
    )
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
    let bb = client(&state, &user).await.map_err(ApiError::BadRequest)?;
    bb.authorize(&query.course)
        .await
        .map_err(ApiError::Forbidden)?;
    bb.assignment(&query.course, &query.column)
        .await
        .map_err(ApiError::BadRequest)?;
    let attempts = bb
        .attempts(&query.course, &query.column)
        .await
        .map_err(ApiError::BadRequest)?;
    let attempt = attempts
        .iter()
        .find(|a| a.id == query.attempt)
        .ok_or(ApiError::NotFound)?;
    let students = bb
        .students(&query.course)
        .await
        .map_err(ApiError::BadRequest)?;
    let student = students.get(&attempt.user_id).ok_or(ApiError::NotFound)?;
    let submission = bb
        .submission(&query.course, &query.column, attempt, student)
        .await
        .map_err(ApiError::BadRequest)?;
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
    let filename = format!("{}_{}_{}", student.number, student.name, file.name);
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
        use std::sync::{Arc, Mutex};
        #[derive(Default)]
        struct Remote {
            score: Option<f64>,
            writes: Vec<(String, String)>,
            uncertain: bool,
        }
        let remote = Arc::new(Mutex::new(Remote::default()));
        let app = Router::new()
            .route("/learn/api/public/v1/users/me",get(||async {Json(json!({"id":"_me_1"}))}))
            .route("/learn/api/public/v1/users/_me_1/courses",get(||async {Json(json!({"results":[{"courseId":"_c_1","courseRoleId":"TeachingAssistant"}]}))}))
            .route("/learn/api/public/v2/courses/_c_1/gradebook/columns",get(||async {Json(json!({"results":[{"id":"_h_1","name":"作业","score":{"possible":100}}]}))}))
            .route("/learn/api/public/v2/courses/_c_1/gradebook/columns/_h_1/attempts",get(|State(r):State<Arc<Mutex<Remote>>>|async move {
                let r=r.lock().unwrap();Json(json!({"results":[
                    {"id":"_a_old","userId":"_s_1","status":"NeedsGrading","attemptDate":"2026-11-10T00:00:00Z"},
                    {"id":"_a_1","userId":"_s_1","status":if r.score.is_some(){"Completed"}else{"NeedsGrading"},"score":r.score,"attemptDate":"2026-11-12T00:00:00Z"}]}))
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
        let attempts = bb.attempts(&s.course, &s.column).await.unwrap();
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
