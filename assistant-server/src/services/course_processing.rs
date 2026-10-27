//! Course continuation, invoked by the completion event consumer rather than a waiting producer.
use super::lesson_storage;
use crate::app_state::AppState;
use std::path::PathBuf;
use uuid::Uuid;

pub async fn complete_lesson(
    state: &AppState,
    task_id: Uuid,
    user_id: Uuid,
    username: &str,
    course_id: &str,
    lesson_id: &str,
) -> Result<(), String> {
    let (notes, artifacts) = {
        let jobs = state.pkuvideo_jobs.read().await;
        let job = jobs.get(&task_id).ok_or("任务不存在")?;
        if job.user_id != user_id
            || job.course_id.as_deref() != Some(course_id)
            || job.lesson_id.as_deref() != Some(lesson_id)
        {
            return Err("任务完成通知与课节不匹配".into());
        }
        (
            job.notes_path.clone().ok_or("视频处理未生成课节笔记")?,
            job.artifacts_dir.clone(),
        )
    };
    let mutation = state.lesson_mutation(user_id).await;
    let _guard = mutation.lock().await;
    lesson_storage::install_lesson_notes(
        username,
        course_id,
        lesson_id,
        &PathBuf::from("./data/pkuvideo_jobs").join(notes),
    )?;
    let lessons = lesson_storage::load_lessons(username, course_id)?;
    let filename = lessons
        .iter()
        .find(|lesson| lesson.id == lesson_id)
        .and_then(|lesson| lesson.tex_filename.as_ref())
        .ok_or("保存后未找到课节笔记")?;
    let installed = PathBuf::from("./course_data")
        .join(username)
        .join(course_id)
        .join(filename);
    {
        let mut jobs = state.pkuvideo_jobs.write().await;
        let job = jobs.get_mut(&task_id).ok_or("任务不存在")?;
        job.course_notes_path = Some(installed.to_string_lossy().into_owned());
        // Keep the task record for ID-based polling; course endpoints serve installed notes.
        job.transcript_path = None;
        job.notes_path = None;
        job.pdf_path = None;
        state
            .store
            .update_pkuvideo_job(job.clone())
            .await
            .map_err(|e| e.to_string())?;
    }
    drop(_guard);
    if let Err(error) = tokio::fs::remove_dir_all(&artifacts).await {
        tracing::warn!(%task_id,error=%error,"课节笔记已安装，清理视频中间资源失败");
    }
    Ok(())
}
