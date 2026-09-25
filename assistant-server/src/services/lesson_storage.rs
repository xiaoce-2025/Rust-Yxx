use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const COURSE_DATA_DIR: &str = "./course_data";

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

fn get_course_dir(username: &str, course_id: &str) -> PathBuf {
    get_user_dir(username).join(course_id)
}

fn get_index_path(username: &str, course_id: &str) -> PathBuf {
    get_course_dir(username, course_id).join("index.json")
}

pub fn init_course_dir(username: &str, course_id: &str) -> Result<PathBuf, String> {
    let course_dir = get_course_dir(username, course_id);
    fs::create_dir_all(&course_dir).map_err(|e| format!("创建课节目录失败: {}", e))?;
    Ok(course_dir)
}

pub fn save_lessons(username: &str, course_id: &str, lessons: &[LessonInfo]) -> Result<(), String> {
    let course_dir = get_course_dir(username, course_id);
    fs::create_dir_all(&course_dir).map_err(|e| format!("创建课节目录失败: {}", e))?;

    let file_path = get_index_path(username, course_id);
    let json_data = serde_json::to_string_pretty(lessons)
        .map_err(|e| format!("序列化课节数据失败: {}", e))?;

    fs::write(&file_path, json_data)
        .map_err(|e| format!("保存课节数据失败: {}", e))?;

    tracing::info!("课节数据已保存到: {}", file_path.display());
    Ok(())
}

pub fn load_lessons(username: &str, course_id: &str) -> Result<Vec<LessonInfo>, String> {
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
