//! Background AI tasks only write local drafts; publishing remains an explicit TA action.
use super::*;
use crate::services::ta_ai::{Annotation, Failure, Job, Mode, Provider, ANNOTATION_PATH};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use uuid::Uuid;

fn path(user: Uuid, course: &str, column: &str) -> Result<PathBuf, String> {
    ta::valid_id(course)?;
    ta::valid_id(column)?;
    Ok(PathBuf::from("ta_ai")
        .join(user.to_string())
        .join(format!("{course}.{column}.json")))
}
async fn save(state: &AppState, user: Uuid, job: &Job) -> Result<(), String> {
    state
        .store
        .save_ta_ai_job(user,
            &path(user, &job.course, &job.column)?,
            job,
        )
        .await
}
pub(super) async fn snapshot(
    state: &AppState,
    user: Uuid,
    course: &str,
    column: &str,
) -> Result<Option<Job>, String> {
    let mut job = state
        .store
        .load_sidecar::<Job>(&path(user, course, column)?)
        .await?;
    if let Some(j) = job.as_mut() {
        if j.active() && !state.ta_ai.exists(user, j.id).await {
            // Read-only normalization avoids a status request overwriting a newly started job.
            j.status = "interrupted".into();
            j.message = "服务已重启，任务中断。已生成的草稿保留，可重新开始并跳过已有草稿。".into();
        }
    }
    Ok(job)
}
pub(super) async fn status(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(s): Query<Selection>,
) -> Response {
    match snapshot(&state, user.id, &s.course, &s.column).await {
        Ok(job) => ([(header::CACHE_CONTROL,"no-store")], Json(serde_json::json!({
            "html": view::ai_status(job.as_ref(), &s), "active":job.as_ref().is_some_and(Job::active)
        }))).into_response(),
        Err(error) => ApiError::BadRequest(error).into_response(),
    }
}
pub(super) async fn start(
    state: &AppState,
    user: &CurrentUser,
    form: &[(String, String)],
    s: &Selection,
) -> Result<String, String> {
    require_maximum(state, user.id, &s.course, &s.column).await?;
    let question = field(form, "question").trim();
    let rubric = field(form, "rubric").trim();
    if question.is_empty() || question.chars().count() > 30000 || rubric.chars().count() > 10000 {
        return Err("请填写题目（最多 30000 字），评分标准最多 10000 字".into());
    }
    let mode = match field(form, "ai_mode") {
        "strict" => Mode::Strict,
        "symbolic" => Mode::Symbolic,
        _ => return Err("请选择有效评分模式".into()),
    };
    let config = state.store.get_config(user.id).await.map_err(|e| e.to_string())?;
    let merged = crate::services::video_jobs::merge_with_user_config(&state.settings, &config);
    let provider = Provider::new(&merged.deepseek)?;
    let annotation = state
        .store
        .load_sidecar::<Annotation>(std::path::Path::new(ANNOTATION_PATH))
        .await?
        .unwrap_or_default();
    annotation.validate()?;
    let job = Job {
        id: Uuid::new_v4(),
        course: s.course.clone(),
        column: s.column.clone(),
        question: question.into(),
        rubric: rubric.into(),
        mode,
        include_graded: false,
        status: "queued".into(),
        message: "等待后台批改；只生成草稿，不直接登分".into(),
        total: 0,
        processed: 0,
        drafted: 0,
        skipped: 0,
        failures: vec![],
        reviews: vec![],
        updated_at: chrono::Utc::now().to_rfc3339(),
    };
    let cancel = state.ta_ai.reserve(user.id, job.id).await?;
    if let Err(error) = save(state, user.id, &job).await {
        state.ta_ai.finish(user.id, job.id).await;
        return Err(error);
    }
    tracing::info!(target:"teaching_ta", job_id=%job.id, mode=job.mode.label(), "AI 批改任务已排队");
    let state = state.clone();
    let user = user.clone();
    tokio::spawn(async move {
        let outer_state = state.clone();
        let outer_job = job.clone();
        let user_id = user.id;
        // Supervisor also releases the per-user reservation if an unexpected panic occurs.
        let handle = tokio::spawn(async move {
            let mut job = job;
            let result = run(&state, &user, &mut job, &provider, &annotation, &cancel).await;
            job.updated_at = chrono::Utc::now().to_rfc3339();
            match result {
                Err(error) => {
                    job.status = "failed".into();
                    job.message = error;
                }
                Ok(()) if cancel.load(Ordering::Relaxed) => {
                    job.status = "cancelled".into();
                    job.message = "已取消，已生成的草稿保留".into();
                }
                Ok(()) => {
                    job.status = "completed".into();
                    job.message = "批改完成，请刷新并核对评分草稿；失败项需人工处理".into();
                }
            }
            tracing::info!(target:"teaching_ta", job_id=%job.id, status=%job.status, drafted=job.drafted, skipped=job.skipped, failed=job.failures.len(), "AI 批改任务结束");
            if save(&state, user.id, &job).await.is_err() {
                tracing::warn!(target:"teaching_ta", "AI 批改最终进度保存失败");
            }
        });
        if handle.await.is_err() {
            let mut failed = outer_state
                .store
                .load_sidecar::<Job>(&path(user_id, &outer_job.course, &outer_job.column).unwrap())
                .await
                .ok()
                .flatten()
                .unwrap_or(outer_job.clone());
            failed.status = "failed".into();
            failed.updated_at = chrono::Utc::now().to_rfc3339();
            failed.message = "后台任务异常中断，已生成的草稿保留".into();
            let _ = save(&outer_state, user_id, &failed).await;
        }
        outer_state.ta_ai.finish(user_id, outer_job.id).await;
    });
    Ok("AI 批改已在后台启动，可以继续查看作业".into())
}
pub(super) async fn cancel(
    state: &AppState,
    user: &CurrentUser,
    form: &[(String, String)],
    s: &Selection,
) -> Result<String, String> {
    let id = field(form, "job_id")
        .parse::<Uuid>()
        .map_err(|_| "任务 ID 无效")?;
    let job = snapshot(state, user.id, &s.course, &s.column)
        .await?
        .ok_or("任务不存在")?;
    if job.id != id {
        return Err("任务已经更新，请刷新后操作".into());
    }
    state.ta_ai.cancel(user.id, id).await;
    Ok("已请求取消，当前请求结束并清理临时文件后停止".into())
}
fn extension(name: &str) -> String {
    name.rsplit('.').next().unwrap_or("").to_ascii_lowercase()
}
fn text_file(ext: &str) -> bool {
    matches!(
        ext,
        "txt"
            | "md"
            | "rs"
            | "py"
            | "c"
            | "cpp"
            | "h"
            | "java"
            | "js"
            | "ts"
            | "json"
            | "csv"
            | "tex"
    )
}
fn document(ext: &str) -> bool {
    matches!(
        ext,
        "pdf"
            | "doc"
            | "docx"
            | "ppt"
            | "pptx"
            | "bmp"
            | "tif"
            | "tiff"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
    )
}
async fn assess(
    bb: &Blackboard,
    provider: &Provider,
    job: &Job,
    maximum: f64,
    attempt: &ta::Attempt,
    cancel: &AtomicBool,
) -> Result<crate::services::ta_ai::Assessment, String> {
    let submission = bb
        .submission(&job.course, attempt)
        .await?;
    if submission.files.iter().filter(|f| {
        let ext = extension(&f.name);
        text_file(&ext) || document(&ext)
    }).count() > 20 {
        return Err("支持的附件超过 20 个，需人工处理；未截断提交内容".into());
    }
    let mut skipped_files = vec![];
    let mut ids = vec![];
    let result = async {
        let mut text = submission.text;
        for (i, file) in submission.files.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Err("任务已取消".into());
            }
            let ext = extension(&file.name);
            if !text_file(&ext) && !document(&ext) {
                skipped_files.push(file.name.clone());
                continue;
            }
            let bytes = bb.download(file).await?;
            if text_file(&ext) {
                if bytes.len() > 1024 * 1024 {
                    return Err("文本附件过大，需人工处理".into());
                }
                text.push_str(&format!(
                    "\n附件 {}：\n{}",
                    i + 1,
                    std::str::from_utf8(&bytes).map_err(|_| "文本附件不是 UTF-8，需人工处理")?
                ));
            } else {
                ids.push(provider.upload(bytes, &ext, i + 1).await?);
            }
        }
        if text.len() > 2 * 1024 * 1024 {
            return Err("提交正文过大，需人工处理；未截断内容".into());
        }
        if text.trim().is_empty() && ids.is_empty() {
            return Err(if skipped_files.is_empty() {
                "空白提交，需人工处理".into()
            } else {
                format!("跳过不支持的附件后无可批改内容，需人工处理：{}", skipped_files.join("、"))
            });
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("任务已取消".into());
        }
        let mut assessment = provider.assess(job, maximum, &text, &ids).await?;
        assessment.skipped_files = skipped_files;
        Ok(assessment)
    }
    .await;
    provider.cleanup(&ids).await;
    result
}
async fn run(
    state: &AppState,
    user: &CurrentUser,
    job: &mut Job,
    provider: &Provider,
    annotation: &Annotation,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let _permit = loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        match tokio::time::timeout(
            std::time::Duration::from_secs(1),
            state.ta_ai.slots.acquire(),
        )
        .await
        {
            Ok(result) => break result.map_err(|_| "后台任务队列不可用")?,
            Err(_) => continue,
        }
    };
    // AI draft creation retains its live concurrency checks; all reads are persisted.
    let bb = Blackboard::live_checks(state, user);
    run_with_client(state, user, job, provider, annotation, cancel, &bb).await
}
async fn run_with_client(
    state: &AppState,
    user: &CurrentUser,
    job: &mut Job,
    provider: &Provider,
    annotation: &Annotation,
    cancel: &Arc<AtomicBool>,
    bb: &Blackboard,
) -> Result<(), String> {
    bb.authorize(&job.course).await?;
    let maximum = require_maximum(state, user.id, &job.course, &job.column).await?;
    let attempts = bb.grading_queue(&job.course).await?.into_iter()
        .find(|a| a.id == job.column).ok_or("该作业当前没有待评分提交")?.attempts;
    job.total = attempts.len();
    job.status = "running".into();
    job.message = "正在读取提交并生成评分草稿".into();
    save(state, user.id, job).await?;
    let mut consecutive_failures = 0;
    let drafts_path = draft_path(user.id, &job.course, &job.column)?;
    for attempt in attempts {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        if state.store.has_grading_draft(&drafts_path, &attempt.id).await.map_err(|error| error.to_string())? {
            job.skipped += 1;
        } else {
            let result: Result<bool, String> = async {
                let assessment = assess(
                    bb,
                    provider,
                    job,
                    maximum,
                    &attempt,
                    cancel,
                )
                .await?;
                if let Some(reason) = assessment.review_reason() {
                    job.reviews.push(crate::services::ta_ai::Review {
                        attempt: attempt.id.clone(),
                        revision: attempt.revision.clone(),
                        label: attempt.student_name.clone(),
                        score: assessment.reference_score(maximum),
                        feedback: assessment.feedback.clone(),
                        reason,
                    });
                    return Ok(false);
                }
                let score = assessment.grade(maximum, job.mode)?;
                let lock = state.ta_mutation(user.id).await;
                let _guard = lock.lock().await;
                if cancel.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                if load_maximum(state, user.id, &job.course, &job.column).await? != Some(maximum) {
                    return Err("作业满分已变化，请重新开始批改".into());
                }
                // Re-read the attempt before creating a draft. The attempt must still be pending and unchanged.
                let fresh = bb.grading_queue(&job.course).await?;
                if !fresh.iter().find(|a| a.id == job.column).is_some_and(|a| {
                    a.attempts.iter().any(|a| a.id == attempt.id && a.revision == attempt.revision)
                }) {
                    return Err("提交或教学网成绩已变化，请重新核对".into());
                }
                let draft = Draft {
                    attempt_id: attempt.id.clone(),
                    student_name: attempt.student_name.clone(),
                    score,
                    feedback: annotation.apply(&assessment.feedback, job.mode),
                    revision: attempt.revision.clone(),
                    updated_at: chrono::Utc::now().to_rfc3339(),
                    synced: false,
                    error: None,
                };
                state.store.save_grading_drafts(
                    &drafts_path, &[draft], false,
                ).await.map(|count| count != 0).map_err(|error| error.to_string())
            }
            .await;
            match result {
                Ok(true) => {
                    job.drafted += 1;
                    consecutive_failures = 0;
                }
                Ok(false) => {
                    if !job.reviews.iter().any(|r| r.attempt == attempt.id) {
                        job.skipped += 1;
                    }
                    consecutive_failures = 0;
                }
                Err(error) => {
                    // Unreadable/unsupported submissions must not prevent grading other students.
                    if error.starts_with("DeepSeek") || error.starts_with("AI ") {
                        consecutive_failures += 1;
                    } else {
                        consecutive_failures = 0;
                    }
                    let fatal = ["HTTP 401", "HTTP 403", "HTTP 402", "HTTP 429"]
                        .iter()
                        .any(|s| error.contains(s));
                    job.failures.push(Failure {
                        attempt: attempt.id,
                        label: attempt.student_name.clone(),
                        message: error,
                    });
                    if fatal || consecutive_failures >= 3 {
                        job.processed += 1;
                        save(state, user.id, job).await?;
                        return Err("接口拒绝请求或连续三次 AI 接口/输出异常，已停止任务。请检查失败项；其余提交未处理。".into());
                    }
                }
            }
        }
        job.processed += 1;
        job.updated_at = chrono::Utc::now().to_rfc3339();
        save(state, user.id, job).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Bytes, routing::delete};
    use serde_json::json;
    use std::{collections::HashMap, sync::Mutex};

    #[derive(Default)]
    struct Remote {
        uploads: usize,
        chats: usize,
        deletes: usize,
        change: bool,
        changed: bool,
        bad_output: bool,
        review: bool,
    }
    fn job() -> Job {
        Job {
            id: Uuid::new_v4(),
            course: "_c_1".into(),
            column: "_h_1".into(),
            question: "第一题计算，第二题证明".into(),
            rubric: "按题评分".into(),
            mode: Mode::Symbolic,
            include_graded: false,
            status: "queued".into(),
            message: String::new(),
            total: 0,
            processed: 0,
            drafted: 0,
            skipped: 0,
            failures: vec![],
            reviews: vec![],
            updated_at: String::new(),
        }
    }
    #[tokio::test]
    async fn one_model_call_for_all_questions_pdf_cleanup_and_drafts_only() {
        let remote = Arc::new(Mutex::new(Remote::default()));
        let app=Router::new()
            .route("/webapps/portal/execute/tabs/tabAction", get(|| async {
                axum::response::Html(r#"<a href=" /webapps/blackboard/execute/launcher?type=Course&amp;id=PkId{key=_c_1, dataType=blackboard.data.course.Course}&amp;url=">编号: 测试课程</a>"#)
            }))
            .route("/webapps/gradebook/do/instructor/viewNeedsGrading", get(|State(r): State<Arc<Mutex<Remote>>>| async move {
                let changed = r.lock().unwrap().changed;
                let rows = [1, 5, 6].into_iter().filter(|&i| i != 1 || !changed)
                    .map(|i| ta::grading_row("_c_1", "_h_1", "多题作业", &format!("_a_{i}"), "2026年11月14日"))
                    .collect::<String>();
                Html(format!("<table>{rows}</table>"))
            }))
            .route("/webapps/assignment/gradeAssignmentRedirector",get(|State(r):State<Arc<Mutex<Remote>>>,Query(q):Query<HashMap<String,String>>|async move{
                assert_eq!(q, HashMap::from([("course_id".into(), "_c_1".into()), ("attempt_id".into(), "_a_1".into())]));
                let mut html = "<div id='submissionText'>第一题：计算答案。第二题：证明。</div><a onclick=\"inlineView(event, '_f_1')\">附件</a>".to_string();
                if r.lock().unwrap().review { html.push_str("<a onclick=\"inlineView(event, '_f_2')\">其他附件</a>"); }
                Html(html)
            }))
            .route("/webapps/assignment/inlineView",get(|Query(q):Query<HashMap<String,String>>|async move{Json(if q["file_id"]=="_f_2" {json!({"fileName":"extra.zip","downloadUrl":"/must-not-download"})} else {json!({"fileName":"PRIVATE-STUDENT.pdf","downloadUrl":"/submission.pdf"})})}))
            .route("/submission.pdf",get(||async{b"%PDF-test with diagrams".as_slice()}))
            .route("/files",post(|State(r):State<Arc<Mutex<Remote>>>, headers:HeaderMap, body:Bytes|async move{
                assert_eq!(headers["authorization"],"Bearer TEST_KEY");assert!(!headers.contains_key("cookie"));
                let body=String::from_utf8_lossy(&body);
                assert!(body.find("name=\"purpose\"").unwrap()<body.find("name=\"file\"").unwrap());
                assert!(body.contains("rich_layout_recognition")&&body.contains("3600")&&body.contains("expires_after[anchor]"));
                assert!(body.contains("submission-1.pdf")&&!body.contains("PRIVATE-STUDENT"));
                r.lock().unwrap().uploads+=1;Json(json!({"id":"file-api-test"}))
            }))
            .route("/chat/completions",post(|State(r):State<Arc<Mutex<Remote>>>,headers:HeaderMap,Json(value):Json<serde_json::Value>|async move{
                assert_eq!(headers["authorization"],"Bearer TEST_KEY");assert!(!headers.contains_key("cookie"));
                assert_eq!(value["model"],"deepseek-flash");assert_eq!(value["response_format"]["type"],"json_object");
                assert_eq!(value["messages"][1]["content"][1],json!({"type":"file","file_id":"file-api-test"}));
                let prompt=value["messages"][1]["content"][0]["text"].as_str().unwrap();
                assert!(prompt.contains("第一题")&&prompt.contains("第二题")&&prompt.contains("满分：50"));
                assert!(!prompt.contains("260001")&&!prompt.contains("TEST_KEY"));
                let mut r=r.lock().unwrap();r.chats+=1;if r.change{r.changed=true;}
                Json(json!({"choices":[{"finish_reason":if r.bad_output{"length"}else{"stop"},"message":{"content":json!({"score":0,"feedback":"第一题计算有误，第二题缺少关键论证。请复核符号并补充推导。","needs_review":r.review,"has_work":true,"error_count":2}).to_string()}}]}))
            }))
            .route("/files/file-api-test",delete(|State(r):State<Arc<Mutex<Remote>>>|async move{r.lock().unwrap().deletes+=1;Json(json!({"deleted":true}))}))
            .fallback(|method:axum::http::Method|async move{assert_eq!(method,axum::http::Method::GET,"AI must never publish a grade");StatusCode::NOT_FOUND})
            .with_state(remote.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let tmp = tempdir::TempDir::new("ta-ai-test").unwrap();
        let settings=serde_json::from_value(json!({"server":{"address":"127.0.0.1","port":0},"wechat":{"api_base_url":"","oauth_app_id":"","oauth_app_secret":"","redirect_uri":"","qr_bot_type":""},"security":{"session_ttl_hours":1},"cors":{"allowed_origins":[],"allow_credentials":false},"audioapi_paraformer":{},"llm_deepseek":{}})).unwrap();
        let state = AppState::new(
            settings,
            Arc::new(
                crate::store::FileStore::new(tmp.path().into())
                    .await
                    .unwrap(),
            ),
        );
        let user = CurrentUser {
            id: Uuid::new_v4(),
            username: "助教".into(),
            email: "test@pku.edu.cn".into(),
        };
        let bb = Blackboard::test_client(&base);
        let provider = Provider::new(&crate::config::DeepSeekConfig {
            api_key: Some("TEST_KEY".into()),
            base_url: Some(base),
            model: None,
        })
        .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        save_maximum(&state, user.id, "_c_1", "_h_1", 50.0).await.unwrap();
        // One existing human draft, plus a synced draft, both protected against AI overwrites.
        let original: Vec<_> = (5..=6)
            .map(|i| Draft {
                attempt_id: format!("_a_{i}"),
                student_name: format!("学生 {i}"),
                score: 30.0,
                feedback: "人工评语".into(),
                revision: "original".into(),
                updated_at: String::new(),
                synced: i == 6,
                error: None,
            })
            .collect();
        persist(&state, user.id, "_c_1", "_h_1", &original)
            .await
            .unwrap();
        let mut j = job();
        run_with_client(
            &state,
            &user,
            &mut j,
            &provider,
            &Annotation::default(),
            &cancel,
            &bb,
        )
        .await
        .unwrap();
        assert_eq!((j.total, j.processed, j.drafted, j.skipped), (3, 3, 1, 2));
        let drafts = load_drafts(&state, user.id, "_c_1", "_h_1").await.unwrap();
        let ai = drafts.iter().find(|d| d.attempt_id == "_a_1").unwrap();
        assert_eq!(ai.score, 49.0);
        assert!(ai.feedback.ends_with("本评语由 AI 生成。"));
        assert!(!ai.synced);
        assert_eq!(drafts[0].feedback, "人工评语");
        assert!(drafts[1].synced);
        assert!(load_drafts(&state, Uuid::new_v4(), "_c_1", "_h_1")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            {
                let r = remote.lock().unwrap();
                (r.uploads, r.chats, r.deletes)
            },
            (1, 1, 1)
        );
        // Malformed/truncated model output still cleans uploaded files and never creates a zero grade.
        state.store.discard_grading_drafts(
            &draft_path(user.id, "_c_1", "_h_1").unwrap(),
            &std::collections::HashSet::from(["_a_1"]),
        ).await.unwrap();
        remote.lock().unwrap().bad_output = true;
        let mut j = job();
        run_with_client(
            &state,
            &user,
            &mut j,
            &provider,
            &Annotation::default(),
            &cancel,
            &bb,
        )
        .await
        .unwrap();
        assert_eq!(j.drafted, 0);
        assert_eq!(j.failures.len(), 1);
        assert_eq!(
            load_drafts(&state, user.id, "_c_1", "_h_1")
                .await
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            {
                let r = remote.lock().unwrap();
                (r.uploads, r.chats, r.deletes)
            },
            (2, 2, 2)
        );
        // Review preserves the actual model score and feedback, skips ZIP before download,
        // cleans PDF uploads, and never manufactures a symbolic minimum score.
        {
            let mut r = remote.lock().unwrap();
            r.bad_output = false;
            r.review = true;
        }
        let mut j = job();
        run_with_client(&state, &user, &mut j, &provider, &Annotation::default(), &cancel, &bb).await.unwrap();
        assert_eq!((j.drafted, j.skipped, j.failures.len(), j.reviews.len()), (0, 2, 0, 1));
        assert_eq!(j.reviews[0].score, Some(0.0));
        assert!(j.reviews[0].reason.contains("extra.zip"));
        assert!(j.reviews[0].feedback.contains("第二题"));
        let html = view::ai_status(Some(&j), &Selection::default());
        assert!(html.contains("AI 参考分数：0") && html.contains("第一题计算有误"));
        assert_eq!({let r=remote.lock().unwrap(); (r.uploads,r.chats,r.deletes)}, (3,3,3));
        // A remote edit while AI is thinking invalidates the result.
        {
            let mut r = remote.lock().unwrap();
            r.review = false;
            r.bad_output = false;
            r.change = true;
        }
        let mut j = job();
        run_with_client(
            &state,
            &user,
            &mut j,
            &provider,
            &Annotation::default(),
            &cancel,
            &bb,
        )
        .await
        .unwrap();
        assert_eq!(j.drafted, 0);
        assert!(j.failures[0].message.contains("已变化"));
        // Cancellation preserves saved drafts and makes no further paid requests.
        cancel.store(true, Ordering::Relaxed);
        let mut j = job();
        run_with_client(
            &state,
            &user,
            &mut j,
            &provider,
            &Annotation::default(),
            &cancel,
            &bb,
        )
        .await
        .unwrap();
        assert_eq!(remote.lock().unwrap().chats, 4);
        // Per-user reservation, scoped status and restart detection.
        let mut j = job();
        save(&state, user.id, &j).await.unwrap();
        assert_eq!(
            snapshot(&state, user.id, "_c_1", "_h_1")
                .await
                .unwrap()
                .unwrap()
                .status,
            "interrupted"
        );
        assert!(snapshot(&state, Uuid::new_v4(), "_c_1", "_h_1")
            .await
            .unwrap()
            .is_none());
        let flag = state.ta_ai.reserve(user.id, j.id).await.unwrap();
        assert!(state.ta_ai.reserve(user.id, Uuid::new_v4()).await.is_err());
        assert!(snapshot(&state, user.id, "_c_1", "_h_1")
            .await
            .unwrap()
            .unwrap()
            .active());
        state.ta_ai.cancel(user.id, j.id).await;
        assert!(flag.load(Ordering::Relaxed));
        state.ta_ai.finish(user.id, j.id).await;
        j.status = "completed".into();
        save(&state, user.id, &j).await.unwrap();
        server.abort();
    }
}
