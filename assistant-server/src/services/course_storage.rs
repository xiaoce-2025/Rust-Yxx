use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::PathBuf};

use super::course_parser::CourseInfo;

const COURSE_DATA_DIR: &str = "./course_data";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCourses {
    pub username: String,
    pub courses: Vec<CourseInfo>,
    pub last_updated: String,
}

pub fn init_storage() -> Result<(), String> {
    fs::create_dir_all(COURSE_DATA_DIR).map_err(|e| format!("创建课程数据目录失败: {}", e))?;
    Ok(())
}

pub fn save_courses(username: &str, courses: &[CourseInfo]) -> Result<(), String> {
    // 确保数据目录存在
    init_storage()?;

    let user_courses = UserCourses {
        username: username.to_string(),
        courses: courses.to_vec(),
        last_updated: chrono::Utc::now().to_rfc3339(),
    };

    let file_path = get_user_file_path(username);
    let json_data = serde_json::to_string_pretty(&user_courses)
        .map_err(|e| format!("序列化课程数据失败: {}", e))?;

    fs::write(&file_path, json_data)
        .map_err(|e| format!("保存课程数据失败: {}", e))?;

    tracing::info!("课程数据已保存到: {}", file_path.display());
    Ok(())
}

pub fn load_courses(username: &str) -> Result<Vec<CourseInfo>, String> {
    let file_path = get_user_file_path(username);
    
    if !file_path.exists() {
        tracing::info!("用户 {} 的课程数据文件不存在", username);
        return Ok(Vec::new());
    }

    let json_data = fs::read_to_string(&file_path)
        .map_err(|e| format!("读取课程数据失败: {}", e))?;

    let user_courses: UserCourses = serde_json::from_str(&json_data)
        .map_err(|e| format!("反序列化课程数据失败: {}", e))?;

    tracing::info!("从本地加载了 {} 门课程", user_courses.courses.len());
    Ok(user_courses.courses)
}

pub fn get_last_updated(username: &str) -> Option<String> {
    let file_path = get_user_file_path(username);
    
    if !file_path.exists() {
        return None;
    }

    match fs::read_to_string(&file_path) {
        Ok(json_data) => {
            match serde_json::from_str::<UserCourses>(&json_data) {
                Ok(user_courses) => Some(user_courses.last_updated),
                Err(_) => None,
            }
        }
        Err(_) => None,
    }
}

fn get_user_file_path(username: &str) -> PathBuf {
    PathBuf::from(COURSE_DATA_DIR).join(format!("{}.json", username))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_save_and_load_courses() {
        let username = "test_user";
        let courses = vec![
            CourseInfo {
                key: "_12345_1".to_string(),
                name: "高等数学".to_string(),
                term: "24-25学年第1学期".to_string(),
            },
            CourseInfo {
                key: "_67890_1".to_string(),
                name: "线性代数".to_string(),
                term: "".to_string(),
            },
        ];

        init_storage().unwrap();
        save_courses(username, &courses).unwrap();
        
        let loaded = load_courses(username).unwrap();
        
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].key, "_12345_1");
        assert_eq!(loaded[1].name, "线性代数");

        // 清理测试文件
        let _ = fs::remove_file(get_user_file_path(username));
    }
}