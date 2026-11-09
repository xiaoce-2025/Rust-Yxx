use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use crate::store::FileStore;

use super::course_parser::CourseInfo;

const COURSE_DATA_DIR: &str = "./course_data";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCourses {
    pub username: String,
    pub courses: Vec<CourseInfo>,
    pub last_updated: String,
}

fn get_user_dir(username: &str) -> PathBuf {
    PathBuf::from(COURSE_DATA_DIR).join(username)
}

fn get_user_courses_path(username: &str) -> PathBuf {
    get_user_dir(username).join("courses.json")
}

pub async fn save_courses(store: &FileStore, username: &str, courses: &[CourseInfo]) -> Result<(), String> {
    super::lesson_order::single_filename(username)?;
    let user_courses = UserCourses {
        username: username.to_string(),
        courses: courses.to_vec(),
        last_updated: chrono::Utc::now().to_rfc3339(),
    };
    let payload = serde_json::to_string(&user_courses).map_err(|error| error.to_string())?;
    store.save_sidecar(&get_user_courses_path(username), &payload).await
}

pub async fn load_catalog(store: &FileStore, username: &str) -> Result<Option<UserCourses>, String> {
    super::lesson_order::single_filename(username)?;
    store.load_sidecar(&get_user_courses_path(username)).await
}

pub async fn load_courses(store: &FileStore, username: &str) -> Result<Vec<CourseInfo>, String> {
    Ok(load_catalog(store, username).await?.map(|catalog| catalog.courses).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_save_and_load_courses() {
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

        let dir = tempdir::TempDir::new_in("target", "course-store").unwrap();
        let store = FileStore::new(dir.path().into()).await.unwrap();
        save_courses(&store, username, &courses).await.unwrap();

        let loaded = load_courses(&store, username).await.unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].key, "_12345_1");
        assert_eq!(loaded[1].name, "线性代数");

    }
}
