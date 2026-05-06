use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CourseInfo {
    pub key: String,
    pub name: String,
    pub term: String,
}

pub fn parse_courses(html_content: &str) -> Vec<CourseInfo> {
    let mut courses = Vec::new();
    
    // 查找所有课程链接
    let re = regex::Regex::new(r#"<a\s+[^>]*href="/webapps/blackboard/execute/launcher\?type=Course&id=PkId\{key=([^,]+)[^>]*>\s*([^<]+)\s*</a>"#).unwrap();
    
    for cap in re.captures_iter(html_content) {
        let key = cap[1].trim().to_string();
        
        let full_text = cap[2].trim();
        
        // 解析课程名和学期
        if let Some((_, after_colon)) = full_text.split_once(':') {
            let text_after_colon = after_colon.trim();
            
            // 查找所有括号，取最后一个作为学期
            let term_re = regex::Regex::new(r"\(([^)]+)\)").unwrap();
            let mut term = String::new();
            let mut name = text_after_colon.to_string();
            
            let mut term_matches: Vec<_> = term_re.captures_iter(text_after_colon).collect();
            if let Some(last_term_cap) = term_matches.pop() {
                term = last_term_cap[1].trim().to_string();
                // 从课程名中移除最后一个括号及其内容
                let term_str = last_term_cap.get(0).unwrap().as_str();
                if let Some(pos) = text_after_colon.rfind(term_str) {
                    name = text_after_colon[0..pos].trim().to_string();
                }
            }
            
            if !key.is_empty() && !name.is_empty() {
                tracing::debug!("解析课程: key={}, name={}, term={}", key, name, term);
                courses.push(CourseInfo {
                    key,
                    name,
                    term,
                });
            }
        }
    }
    
    tracing::info!("共解析出 {} 门课程", courses.len());
    courses
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_course() {
        let html = r#"
            <a href="/webapps/blackboard/execute/launcher?type=Course&id=PkId{key=_98872_1, dataType=blackboard.data.course.Course, container=blackboard.persist.DatabaseContainer@68ec8a74}&url="
               target="_top">25262-00225-22530003-170****303-00-1: AI中的数学(25-26学年第2学期)</a>
        "#;
        
        let courses = parse_courses(html);
        
        assert_eq!(courses.len(), 1);
        assert_eq!(courses[0].key, "_98872_1");
        assert_eq!(courses[0].name, "AI中的数学");
        assert_eq!(courses[0].term, "25-26学年第2学期");
    }

    #[test]
    fn test_parse_course_with_multiple_brackets() {
        let html = r#"
            <a href="/webapps/blackboard/execute/launcher?type=Course&id=PkId{key=_12345_1, dataType=blackboard.data.course.Course}&url=">xxx: 中国通史 (古代部分)(25-26学年第1学期)</a>
        "#;
        
        let courses = parse_courses(html);
        
        assert_eq!(courses.len(), 1);
        assert_eq!(courses[0].key, "_12345_1");
        assert_eq!(courses[0].name, "中国通史 (古代部分)");
        assert_eq!(courses[0].term, "25-26学年第1学期");
    }

    #[test]
    fn test_parse_multiple_courses() {
        let html = r#"
            <a href="/webapps/blackboard/execute/launcher?type=Course&id=PkId{key=_12345_1, dataType=blackboard.data.course.Course}&url=">xxx: 高等数学(24-25学年第1学期)</a>
            <a href="/webapps/blackboard/execute/launcher?type=Course&id=PkId{key=_67890_1, dataType=blackboard.data.course.Course}&url=">yyy: 线性代数</a>
        "#;
        
        let courses = parse_courses(html);
        
        assert_eq!(courses.len(), 2);
        assert_eq!(courses[0].key, "_12345_1");
        assert_eq!(courses[0].name, "高等数学");
        assert_eq!(courses[0].term, "24-25学年第1学期");
        
        assert_eq!(courses[1].key, "_67890_1");
        assert_eq!(courses[1].name, "线性代数");
        assert_eq!(courses[1].term, "");
    }
}
