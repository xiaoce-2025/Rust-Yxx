use super::*;
use crate::{
    services::{course_storage, lesson_storage},
    store::{OperationsQuery, SiteFeatures, FEATURES_PATH},
};
use axum::extract::Query;
use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path as FilePath, PathBuf},
};

#[derive(Default, Deserialize)]
pub(super) struct ListQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    user: String,
    #[serde(default)]
    page: u32,
}
pub(super) async fn records(
    State(state): State<AppState>,
    Query(q): Query<OperationsQuery>,
) -> Result<Json<Value>, ApiError> {
    if !matches!(q.kind.as_str(), "ai_jobs" | "activity" | "credits")
        || q.q.len() > 300
        || q.status.len() > 40
        || q.event.len() > 40
        || (!q.user.is_empty() && Uuid::parse_str(&q.user).is_err())
        || matches!((q.from, q.before), (Some(from), Some(before)) if from >= before)
    {
        return Err(ApiError::BadRequest("查询参数无效".into()));
    }
    Ok(Json(
        state
            .store
            .operations_page(&q)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?,
    ))
}
pub(super) async fn ai_record(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        state
            .store
            .ai_job_record(id)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
            .ok_or(ApiError::NotFound)?,
    ))
}
pub(super) async fn features(
    State(state): State<AppState>,
) -> Result<Json<SiteFeatures>, ApiError> {
    Ok(Json(
        state
            .store
            .site_features()
            .await
            .map_err(ApiError::Internal)?,
    ))
}
pub(super) async fn save_features(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(features): Json<SiteFeatures>,
) -> Result<Json<Value>, ApiError> {
    state
        .store
        .save_admin_setting(
            user.id,
            FilePath::new(FEATURES_PATH),
            &serde_json::to_string(&features).unwrap(),
        )
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({"success":true})))
}
fn component(s: &str) -> Result<(), ApiError> {
    if s.is_empty()
        || s.contains(['\\', '/', ':'])
        || !matches!(
            (
                FilePath::new(s).components().next(),
                FilePath::new(s).components().count()
            ),
            (Some(Component::Normal(_)), 1)
        )
    {
        return Err(ApiError::BadRequest("笔记标识无效".into()));
    }
    Ok(())
}
pub(super) async fn notes(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    if q.q.len() > 300 {
        return Err(ApiError::BadRequest("搜索内容过长".into()));
    }
    let users = state.store.admin_users().await;
    let paths = state
        .store
        .sidecar_paths_under(FilePath::new("./course_data"))
        .await
        .map_err(ApiError::Internal)?;
    let mut records = vec![];
    let mut warnings = vec![];
    for user in &users {
        if !q.user.is_empty() && q.user != user.id.to_string() {
            continue;
        }
        let username = user.email.split('@').next().unwrap_or("");
        if component(username).is_err() {
            continue;
        }
        let catalog = course_storage::load_courses(&state.store, username)
            .await
            .map_err(ApiError::Internal)?;
        let names: HashMap<_, _> = catalog
            .iter()
            .map(|c| (c.key.as_str(), c.name.as_str()))
            .collect();
        let mut courses: HashSet<String> = paths
            .iter()
            .filter_map(|p| p.strip_prefix(&format!("{username}/")))
            .filter_map(|p| p.strip_suffix("/index.json"))
            .filter(|p| component(p).is_ok())
            .map(str::to_owned)
            .collect();
        let dir = PathBuf::from("./course_data").join(username);
        if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?
            {
                if entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                    if let Some(name) = entry.file_name().to_str() {
                        if component(name).is_ok() {
                            courses.insert(name.into());
                        }
                    }
                }
            }
        }
        for course in courses {
            let lessons = match lesson_storage::load_lessons(&state.store, username, &course).await
            {
                Ok(v) => v,
                Err(_) => {
                    warnings.push(format!("{} / {} 的课节索引无法读取", user.username, course));
                    continue;
                }
            };
            for lesson in lessons.iter().filter(|l| l.tex_filename.is_some()) {
                records.push(json!({"id":format!("course:{}:{course}:{}",user.id,lesson.id),"user_id":user.id,"username":user.username,"course":course,"title":names.get(course.as_str()).copied().unwrap_or(&course),"lesson":lesson.id,"date":lesson.date,"kind":"course","format":"tex / pdf"}));
            }
        }
    }
    for job in state
        .store
        .get_all_pkuvideo_jobs()
        .await
        .values()
        .filter(|j| j.course_id.is_none() && (j.notes_path.is_some() || j.pdf_path.is_some()))
    {
        if !q.user.is_empty() && q.user != job.user_id.to_string() {
            continue;
        }
        records.push(json!({"id":format!("job:{}",job.id),"job":job.id,"user_id":job.user_id,"username":users.iter().find(|u|u.id==job.user_id).map(|u|u.username.as_str()).unwrap_or("已删除用户"),"title":"独立笔记任务","date":job.updated_at,"kind":"job","has_tex":job.notes_path.is_some(),"has_pdf":job.pdf_path.is_some() || job.notes_path.is_some()}));
    }
    let search = q.q.to_lowercase();
    records.retain(|r| r.to_string().to_lowercase().contains(&search));
    records.sort_by(|a, b| {
        b["date"]
            .as_str()
            .cmp(&a["date"].as_str())
            .then(a["id"].as_str().cmp(&b["id"].as_str()))
    });
    let total = records.len();
    let page = q.page.max(1);
    let records: Vec<_> = records
        .into_iter()
        .skip((page as usize - 1).saturating_mul(50))
        .take(50)
        .collect();
    Ok(Json(
        json!({"records":records,"total":total,"page":page,"page_size":50,"warnings":warnings}),
    ))
}
#[derive(Default, Deserialize)]
pub(super) struct NoteQuery {
    owner: Uuid,
    #[serde(default)]
    course: String,
    #[serde(default)]
    lesson: String,
    #[serde(default)]
    job: Option<Uuid>,
    #[serde(default)]
    format: String,
}
async fn safe_file(root: &FilePath, candidate: &FilePath) -> Result<PathBuf, ApiError> {
    let root = tokio::fs::canonicalize(root)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let path = tokio::fs::canonicalize(candidate)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if !path.starts_with(&root) {
        return Err(ApiError::Forbidden("笔记文件不在允许的目录中".into()));
    }
    Ok(path)
}
pub(super) async fn note(
    State(state): State<AppState>,
    Query(q): Query<NoteQuery>,
) -> Result<Response, ApiError> {
    if !matches!(q.format.as_str(), "tex" | "pdf") {
        return Err(ApiError::BadRequest("请选择 tex 或 pdf".into()));
    }
    let path = if let Some(id) = q.job {
        let job = state
            .store
            .get_pkuvideo_job(id)
            .await
            .ok_or(ApiError::NotFound)?;
        if job.user_id != q.owner || job.course_id.is_some() {
            return Err(ApiError::NotFound);
        }
        if job.result_tex_sha256.is_some() {
            let path = state.store.user_tex_path(job.user_id, job.id);
            if q.format == "pdf" {
                let bytes = crate::routes::pkuvideo::compile_note_pdf(&path).await?;
                return Ok(([("Content-Type","application/pdf"),("Content-Disposition","inline; filename=notes.pdf")],bytes).into_response());
            }
            let bytes = tokio::fs::read(path).await.map_err(|_|ApiError::NotFound)?;
            return Ok(([("Content-Type","text/plain; charset=utf-8"),("X-Content-Type-Options","nosniff")],bytes).into_response());
        }
        let relative = if q.format == "pdf" {
            job.pdf_path
        } else {
            job.notes_path
        }
        .ok_or(ApiError::NotFound)?;
        let root = FilePath::new("./data/pkuvideo_jobs");
        match safe_file(root, &root.join(&relative)).await {
            Ok(p) => p,
            Err(_) => safe_file(root, FilePath::new(&relative)).await?,
        }
    } else {
        component(&q.course)?;
        component(&q.lesson)?;
        let user = state
            .store
            .get_user(q.owner)
            .await
            .ok_or(ApiError::NotFound)?;
        let username = user.email.split('@').next().unwrap_or("");
        component(username)?;
        let lessons = lesson_storage::load_lessons(&state.store, username, &q.course)
            .await
            .map_err(ApiError::Internal)?;
        let filename = lessons
            .iter()
            .find(|l| l.id == q.lesson)
            .and_then(|l| l.tex_filename.as_ref())
            .ok_or(ApiError::NotFound)?;
        component(filename)?;
        let dir = PathBuf::from("./course_data")
            .join(username)
            .join(&q.course);
        let path = safe_file(FilePath::new("./course_data"), &dir.join(filename)).await?;
        if q.format == "pdf" {
            let owner = CurrentUser {
                id: user.id,
                username: user.username,
                email: user.email,
            };
            let mut response = crate::routes::pkuvideofullclass::download_lesson_pdf(
                State(state),
                axum::extract::Path((q.course, q.lesson)),
                owner,
            )
            .await?
            .into_response();
            response.headers_mut().insert(
                axum::http::header::CONTENT_DISPOSITION,
                HeaderValue::from_static("inline; filename=notes.pdf"),
            );
            return Ok(response);
        }
        path
    };
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if !metadata.is_file() || metadata.len() > 32 * 1024 * 1024 {
        return Err(ApiError::BadRequest("笔记不是普通文件或超过 32 MiB".into()));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| ApiError::NotFound)?;
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                if q.format == "pdf" {
                    "application/pdf"
                } else {
                    "text/plain; charset=utf-8"
                },
            ),
            (axum::http::header::CONTENT_DISPOSITION, "inline"),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
}
