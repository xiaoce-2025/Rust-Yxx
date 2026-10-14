use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const COURSE_DATA_DIR: &str = "./course_data";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualAttempt {
    pub date: String,
    pub status: String,
    pub message: String,
    pub updated_at: String,
}

pub fn should_query_date(attempts: &[ManualAttempt], date: &str, retry_failed: bool) -> bool {
    match attempts.iter().find(|a| a.date == date) {
        None => true,
        Some(a) => retry_failed && a.status != "success",
    }
}

pub fn load_manual_attempts(username: &str, course_id: &str) -> Result<Vec<ManualAttempt>, String> {
    validate_storage_keys(username,course_id)?;
    let path = get_course_dir(username, course_id).join("manual_attempts.json");
    if !path.exists() { return Ok(Vec::new()); }
    serde_json::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

pub fn save_manual_attempts(username: &str, course_id: &str, attempts: &[ManualAttempt]) -> Result<(), String> {
    let dir = init_course_dir(username, course_id)?;
    let data = serde_json::to_vec_pretty(attempts).map_err(|e| e.to_string())?;
    atomic_write(&dir.join("manual_attempts.json"), &data)
}

pub(super) fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&temp, bytes).map_err(|e| e.to_string())?;
    fs::rename(&temp, path).map_err(|e| { let _ = fs::remove_file(&temp); e.to_string() })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LessonInfo {
    pub id: String,
    pub video_url: String,
    pub play_url: String,
    pub date: String,
    pub time: String,
    pub day_of_week: String,
    pub is_processed: bool,
    pub tex_filename: Option<String>,
    #[serde(default)]
    pub manual_replay: Option<super::app_replay::ManualReplay>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CourseLessons {
    pub course_id: String,
    pub course_key: String,
    pub course_name: String,
    pub lessons: Vec<LessonInfo>,
    pub last_updated: String,
}

fn get_user_dir(username: &str) -> PathBuf {
    PathBuf::from(COURSE_DATA_DIR).join(username)
}

fn validate_storage_keys(username: &str, course_id: &str) -> Result<(),String> {
    super::lesson_order::single_filename(username)?;
    super::lesson_order::single_filename(course_id)
}

fn get_course_dir(username: &str, course_id: &str) -> PathBuf {
    get_user_dir(username).join(course_id)
}

fn get_index_path(username: &str, course_id: &str) -> PathBuf {
    get_course_dir(username, course_id).join("index.json")
}

pub fn init_course_dir(username: &str, course_id: &str) -> Result<PathBuf, String> {
    validate_storage_keys(username,course_id)?;
    let course_dir = get_course_dir(username, course_id);
    fs::create_dir_all(&course_dir).map_err(|e| format!("创建课节目录失败: {}", e))?;
    Ok(course_dir)
}

pub fn save_lessons(username: &str, course_id: &str, lessons: &[LessonInfo]) -> Result<(), String> {
    validate_storage_keys(username,course_id)?;
    let course_dir = get_course_dir(username, course_id);
    fs::create_dir_all(&course_dir).map_err(|e| format!("创建课节目录失败: {}", e))?;

    let file_path = get_index_path(username, course_id);
    let json_data = serde_json::to_string_pretty(lessons)
        .map_err(|e| format!("序列化课节数据失败: {}", e))?;

    atomic_write(&file_path, json_data.as_bytes())
        .map_err(|e| format!("保存课节数据失败: {}", e))?;

    tracing::info!("课节数据已保存到: {}", file_path.display());
    Ok(())
}

pub fn load_lessons(username: &str, course_id: &str) -> Result<Vec<LessonInfo>, String> {
    validate_storage_keys(username,course_id)?;
    let _guard = super::lesson_order::STORAGE_LOCK.lock().map_err(|_| "课节存储锁异常")?;
    super::lesson_order::recover(&get_course_dir(username, course_id))?;
    let file_path = get_index_path(username, course_id);

    if !file_path.exists() {
        tracing::info!("用户 {} 课程 {} 的课节数据文件不存在", username, course_id);
        return Ok(Vec::new());
    }

    let json_data = fs::read_to_string(&file_path)
        .map_err(|e| format!("读取课节数据失败: {}", e))?;

    let lessons: Vec<LessonInfo> = serde_json::from_str(&json_data)
        .map_err(|e| format!("反序列化课节数据失败: {}", e))?;

    tracing::info!("从本地加载了 {} 个课节", lessons.len());
    Ok(lessons)
}

pub fn save_ordered_lessons(username: &str, course_id: &str, old: &[LessonInfo], lessons: &mut Vec<LessonInfo>) -> Result<(), String> {
    validate_storage_keys(username,course_id)?;
    super::lesson_order::reconcile(&get_course_dir(username,course_id),course_id,old,lessons)
}

pub fn install_lesson_notes(username: &str, course_id: &str, lesson_id: &str, source: &std::path::Path) -> Result<(), String> {
    let old = load_lessons(username,course_id)?;
    let mut lessons = old.clone();
    let lesson = lessons.iter_mut().find(|l|l.id==lesson_id).ok_or("课节不存在，无法保存笔记")?;
    let dir = init_course_dir(username,course_id)?;
    let pending = format!("note_{}.tex",uuid::Uuid::new_v4().simple());
    let path = dir.join(&pending);
    atomic_write(&path,&fs::read(source).map_err(|e|format!("读取处理结果失败: {e}"))?)?;
    lesson.tex_filename=Some(pending);
    lesson.is_processed=true;
    if let Err(error)=save_ordered_lessons(username,course_id,&old,&mut lessons){
        // Recovery normally restores this newly-created file. If recovery itself
        // failed, keep it as part of the transaction instead of losing notes.
        if !dir.join(".lesson-order").exists(){let _=fs::remove_file(path);}
        return Err(error);
    }
    Ok(())
}

pub fn delete_manual_lesson(username: &str, course_id: &str, lesson_id: &str, confirmation: &str) -> Result<(), String> {
    if confirmation != "确认删除" { return Err("请输入“确认删除”完成二次确认".into()); }
    let old = load_lessons(username,course_id)?;
    let lesson = old.iter().find(|l|l.id==lesson_id).ok_or("课节不存在")?;
    if lesson.manual_replay.is_none() { return Err("仅可删除手动添加的课节".into()); }
    let mut lessons:Vec<_> = old.iter().filter(|l|l.id!=lesson_id).cloned().collect();
    save_ordered_lessons(username,course_id,&old,&mut lessons)
}

pub fn update_lesson_status(
    username: &str,
    course_id: &str,
    lesson_id: &str,
    is_processed: bool,
    tex_filename: Option<String>,
) -> Result<(), String> {
    let mut lessons = load_lessons(username, course_id)?;

    if let Some(lesson) = lessons.iter_mut().find(|l| l.id == lesson_id) {
        lesson.is_processed = is_processed;
        lesson.tex_filename = tex_filename;
    }

    save_lessons(username, course_id, &lessons)
}

pub fn get_lesson_count(username: &str, course_id: &str) -> (usize, usize) {
    match load_lessons(username, course_id) {
        Ok(lessons) => {
            let total = lessons.len();
            let processed = lessons.iter().filter(|l| l.is_processed).count();
            (total, processed)
        }
        Err(_) => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestUser(String);
    impl Drop for TestUser { fn drop(&mut self){let _=fs::remove_dir_all(get_user_dir(&self.0));} }
    #[test]
    fn manual_delete_requires_typed_confirmation_and_keeps_query_markers(){
        let user=TestUser(format!("test_manual_delete_{}",uuid::Uuid::new_v4().simple()));let course="c";
        let mut old=Vec::new();let dir=init_course_dir(&user.0,course).unwrap();
        for(i,id)in ["manual","regular"].iter().enumerate(){let name=super::super::lesson_order::filename(course,i+1);fs::write(dir.join(&name),id).unwrap();old.push(LessonInfo{id:id.to_string(),video_url:String::new(),play_url:String::new(),date:format!("2026-01-0{}",i+1),time:"08:00".into(),day_of_week:String::new(),is_processed:true,tex_filename:Some(name),manual_replay:if i==0{Some(super::super::app_replay::ManualReplay{course_id:"app".into(),sub_id:"sub".into(),date:"2026-01-01".into(),lesson_start:Some(1),lesson_end:Some(2)})}else{None}});}
        save_lessons(&user.0,course,&old).unwrap();
        let attempt=ManualAttempt{date:old[0].date.clone(),status:"success".into(),message:"已添加".into(),updated_at:String::new()};save_manual_attempts(&user.0,course,&[attempt]).unwrap();
        assert!(delete_manual_lesson(&user.0,course,"manual","删除").is_err());
        assert!(delete_manual_lesson(&user.0,course,"regular","确认删除").is_err());
        assert_eq!(load_lessons(&user.0,course).unwrap().len(),2);
        delete_manual_lesson(&user.0,course,"manual","确认删除").unwrap();
        let remaining=load_lessons(&user.0,course).unwrap();assert_eq!(remaining.len(),1);assert_eq!(remaining[0].id,"regular");
        assert_eq!(remaining[0].tex_filename.as_deref(),Some("c_lesson_1.tex"));assert_eq!(fs::read_to_string(dir.join("c_lesson_1.tex")).unwrap(),"regular");
        assert!(!should_query_date(&load_manual_attempts(&user.0,course).unwrap(),"2026-01-01",false));
    }
    #[test]
    fn newly_generated_notes_use_sequence_instead_of_stable_lesson_id(){
        let user=TestUser(format!("test_install_notes_{}",uuid::Uuid::new_v4().simple()));let course="c";let dir=init_course_dir(&user.0,course).unwrap();
        fs::write(dir.join("legacy_uuid.tex"),"old-note").unwrap();
        let mut old:Vec<LessonInfo>=serde_json::from_value(serde_json::json!([
            {"id":"app_uuid_new","video_url":"","play_url":"","date":"2026-01-01","time":"08:00","day_of_week":"","is_processed":false,"tex_filename":null},
            {"id":"stable_old","video_url":"","play_url":"","date":"2026-01-02","time":"08:00","day_of_week":"","is_processed":true,"tex_filename":"legacy_uuid.tex"}])).unwrap();
        save_lessons(&user.0,course,&old).unwrap();let source=get_user_dir(&user.0).join("incoming.tex");fs::write(&source,"new-note").unwrap();
        install_lesson_notes(&user.0,course,"app_uuid_new",&source).unwrap();old=load_lessons(&user.0,course).unwrap();
        assert_eq!(old[0].id,"app_uuid_new");assert_eq!(old[0].tex_filename.as_deref(),Some("c_lesson_1.tex"));assert!(old[0].is_processed);
        assert_eq!(old[1].tex_filename.as_deref(),Some("c_lesson_2.tex"));assert_eq!(fs::read_to_string(dir.join("c_lesson_1.tex")).unwrap(),"new-note");assert_eq!(fs::read_to_string(dir.join("c_lesson_2.tex")).unwrap(),"old-note");
    }

    #[test]
    fn manual_attempts_skip_success_failure_and_interruption_until_explicit_retry() {
        let records: Vec<ManualAttempt> = ["success", "failed", "pending"].iter().enumerate()
            .map(|(i,status)| ManualAttempt { date:format!("2026-10-{}", 10+i), status:status.to_string(),
                message:String::new(), updated_at:String::new() }).collect();
        for date in ["2026-10-10", "2026-10-11", "2026-10-12"] {
            assert!(!should_query_date(&records, date, false));
        }
        assert!(!should_query_date(&records, "2026-10-10", true));
        assert!(should_query_date(&records, "2026-10-11", true));
        assert!(should_query_date(&records, "2026-10-12", true));
        assert!(should_query_date(&records, "2026-10-13", false));
    }

    #[test]
    fn old_lesson_json_remains_readable() {
        let lesson: LessonInfo = serde_json::from_str(r#"{"id":"lesson_1","video_url":"","play_url":"",
            "date":"2026-10-13","time":"08:00","day_of_week":"周二","is_processed":false,"tex_filename":null}"#).unwrap();
        assert!(lesson.manual_replay.is_none());
    }

    #[test]
    fn test_save_and_load_lessons() {
        let username = "test_user";
        let course_id = "_12345_1";
        let lessons = vec![
            LessonInfo {
                id: "1".to_string(),
                video_url: "http://example.com/video1.mp4".to_string(),
                play_url: "http://example.com/watch/1".to_string(),
                date: "2025-03-01".to_string(),
                time: "09:00".to_string(),
                day_of_week: "周六".to_string(),
                is_processed: false,
                tex_filename: None,
                manual_replay: None,
            },
            LessonInfo {
                id: "2".to_string(),
                video_url: "http://example.com/video2.mp4".to_string(),
                play_url: "http://example.com/watch/2".to_string(),
                date: "2025-03-08".to_string(),
                time: "09:00".to_string(),
                day_of_week: "周六".to_string(),
                is_processed: true,
                tex_filename: Some("lesson_2.tex".to_string()),
                manual_replay: None,
            },
        ];

        init_course_dir(username, course_id).unwrap();
        save_lessons(username, course_id, &lessons).unwrap();

        let loaded = load_lessons(username, course_id).unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "1");
        assert_eq!(loaded[1].is_processed, true);

        // 清理测试文件
        let _ = fs::remove_dir_all(get_user_dir(username));
    }
}
