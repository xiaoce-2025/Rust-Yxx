//! Native Blackboard TA client. Authentication is owned by the existing teaching login cache.
use super::{course_parser, pkulogin::PkuLoginResult};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

const BASE: &str = "https://course.pku.edu.cn";
const COURSE_PORTAL: &str = "/webapps/portal/execute/tabs/tabAction?tab_tab_group_id=_1_1";
const MAX_PAGES: usize = 100;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Course {
    pub id: String,
    pub name: String,
    pub term: String,
}
#[derive(Clone, Debug)]
pub struct Assignment {
    pub id: String,
    pub name: String,
    pub possible: f64,
}
#[derive(Clone, Debug)]
pub struct Group {
    pub id: String,
    pub name: String,
}
#[derive(Clone, Debug)]
pub struct Student {
    pub membership: String,
    pub number: String,
    pub name: String,
}
#[derive(Clone, Debug)]
pub struct Attempt {
    pub id: String,
    pub user_id: String,
    pub date: String,
    pub status: String,
    pub score: Option<f64>,
    pub revision: String,
    pub exempt: bool,
}

/// Blackboard may return a placeholder score of 0 before manual grading.
/// Its explicit attempt status takes precedence over the numeric score.
pub fn attempt_is_graded(status: &str, score: Option<f64>) -> bool {
    let status = status
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    match status.as_str() {
        "completed" | "graded" => true,
        "needsgrading" | "needsgradingagain" | "needsmanualgrading" | "notattempted"
        | "inprogress" | "inprogressagain" | "suspended" | "abandoned" | "submitted"
        | "ungraded" | "pending" => false,
        // Unknown/missing status + zero is ambiguous, not proof of a grade.
        _ => score.is_some_and(|score| score.is_finite() && score != 0.0),
    }
}

fn attempt_score(value: &Value) -> Option<f64> {
    let score = value
        .get("score")
        .and_then(Value::as_f64)
        .or_else(|| value.pointer("/grade/score").and_then(Value::as_f64));
    if attempt_is_graded(string(value, "status"), score) {
        score
    } else {
        None
    }
}
#[derive(Clone, Debug)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub url: String,
}
#[derive(Clone, Debug, Default)]
pub struct Submission {
    pub text: String,
    pub files: Vec<Attachment>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Draft {
    pub attempt_id: String,
    pub student_id: String,
    pub score: f64,
    pub feedback: String,
    pub revision: String,
    pub updated_at: String,
    #[serde(default)]
    pub synced: bool,
    #[serde(default)]
    pub error: Option<String>,
}

pub struct Blackboard {
    base: Url,
    // Keep the existing cookie-file lease alive for the whole operation.
    login: Option<PkuLoginResult>,
    cached: Option<CachedClient>,
}

struct CachedClient {
    state: crate::app_state::AppState,
    user: crate::auth::CurrentUser,
    login: tokio::sync::OnceCell<PkuLoginResult>,
    fresh: bool,
    refresh_submissions: bool,
}

#[derive(Serialize, Deserialize)]
struct CachedResponse {
    // Base64 keeps binary attachments compact in the existing SQLite sidecar store.
    body: String,
}

impl Blackboard {
    #[cfg(test)]
    pub(crate) fn test_client(base: &str) -> Self {
        let cookie = std::env::temp_dir().join(format!("ta-test-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&cookie, "# Netscape HTTP Cookie File\n").unwrap();
        Self {
            base: Url::parse(base).unwrap(),
            login: Some(PkuLoginResult::new(cookie, "Windows TA-test".into())),
            cached: None,
        }
    }
    pub fn from_session(login: PkuLoginResult) -> Result<Self, String> {
        if !login.cookie_file.is_file() {
            return Err("无法读取教学网缓存，请在用户中心检查教学网绑定".into());
        }
        Ok(Self {
            base: Url::parse(BASE).unwrap(),
            login: Some(login),
            cached: None,
        })
    }

    /// No authentication or network work until an uncached read is requested.
    pub fn cached(state: &crate::app_state::AppState, user: &crate::auth::CurrentUser, fresh: bool) -> Self {
        Self {
            base: Url::parse(BASE).unwrap(),
            login: None,
            cached: Some(CachedClient {
                state: state.clone(), user: user.clone(),
                login: tokio::sync::OnceCell::new(), fresh,
                refresh_submissions: fresh,
            }),
        }
    }

    /// Validate changing grade metadata live while reusing already downloaded work.
    pub fn live_checks(state: &crate::app_state::AppState, user: &crate::auth::CurrentUser) -> Self {
        let mut client = Self::cached(state, user, true);
        client.cached.as_mut().unwrap().refresh_submissions = false;
        client
    }

    async fn session(&self) -> Result<PkuLoginResult, String> {
        if let Some(login) = &self.login { return Ok(login.clone()); }
        let cache = self.cached.as_ref().ok_or("教学网缓存不可用")?;
        cache.login.get_or_try_init(|| async {
            let stored = cache.state.store.get_user(cache.user.id).await.ok_or("用户不存在")?;
            super::teaching_session::login(&cache.state, &stored).await
        }).await.cloned()
    }

    fn cache_path(&self, path: &str) -> Result<std::path::PathBuf, String> {
        let cache = self.cached.as_ref().ok_or("教学网缓存不可用")?;
        let url = self.url(path)?;
        Ok(std::path::PathBuf::from("ta_cache").join(cache.user.id.to_string())
            .join(format!("{:x}.json", Sha256::digest(url.as_str()))))
    }

    async fn cached_get(&self, path: &str, fresh: bool) -> Result<Vec<u8>, String> {
        self.cached_get_version(path, fresh, "").await
    }

    async fn cached_get_version(&self, path: &str, fresh: bool, version: &str) -> Result<Vec<u8>, String> {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let Some(cache) = &self.cached else { return self.request(path, Vec::new(), None).await; };
        let mut key = self.cache_path(path)?;
        if !version.is_empty() {
            key.set_extension(format!("{:x}.json", Sha256::digest(version)));
        }
        if !fresh {
            if let Some(saved) = cache.state.store.load_sidecar::<CachedResponse>(&key).await? {
                return STANDARD.decode(saved.body).map_err(|_| "教学网本地缓存损坏，请点击刷新".into());
            }
        }
        let body = self.request(path, Vec::new(), None).await?;
        let saved = serde_json::to_string(&CachedResponse { body: STANDARD.encode(&body) })
            .map_err(|_| "无法编码教学网本地缓存")?;
        cache.state.store.save_sidecar(&key, &saved).await?;
        Ok(body)
    }

    fn url(&self, path: &str) -> Result<Url, String> {
        let url = self.base.join(path).map_err(|_| "教学网返回了无效地址")?;
        if !same_origin(&self.base, &url) || !url.username().is_empty() || url.password().is_some()
        {
            return Err("已拒绝非教学网地址".into());
        }
        Ok(url)
    }

    async fn bytes(
        &self,
        response: super::pkulogin::TeachingResponse,
        url: &Url,
    ) -> Result<Vec<u8>, String> {
        let route = safe_route(url);
        tracing::info!(target: "teaching_ta", route=%route, status=response.status, "教学网响应");
        let login_redirect = (300..400).contains(&response.status)
            && response
                .location
                .as_deref()
                .is_some_and(|s| s.contains("iaaa.pku.edu.cn") || s.contains("/webapps/login"));
        if response.status == 401 || login_redirect || url.path().contains("/webapps/login") {
            if let Some(login) = &self.login {
                login.invalidate();
            }
            if let Some(login) = self.cached.as_ref().and_then(|c| c.login.get()) { login.invalidate(); }
            return Err("教学网登录已失效，请刷新重试；将复用现有登录逻辑".into());
        }
        if response.status == 403 {
            return Err("教学网拒绝访问：当前账号没有此项助教权限".into());
        }
        if !(200..300).contains(&response.status) {
            return Err(format!("教学网请求失败（HTTP {}）", response.status));
        }
        let result = response.body;
        // A successful HTTP status can still carry a login page.
        if let Ok(text) = std::str::from_utf8(&result) {
            if text.trim_start().starts_with('<') {
                if let Err(error) = super::pkulogin::validate_page(text) {
                    if super::pkulogin::is_auth_error(&error) {
                        if let Some(login) = &self.login {
                            login.invalidate();
                        }
                        if let Some(login) = self.cached.as_ref().and_then(|c| c.login.get()) { login.invalidate(); }
                    }
                    return Err(error);
                }
            }
        }
        Ok(result)
    }

    async fn request(
        &self,
        path: &str,
        headers: Vec<(String, String)>,
        form: Option<String>,
    ) -> Result<Vec<u8>, String> {
        let mut url = self.url(path)?;
        let login = self.session().await?;
        for _ in 0..8 {
            let route = safe_route(&url);
            tracing::info!(target: "teaching_ta", method=if form.is_some() { "POST" } else { "GET" }, route=%route, "请求教学网");
            let (session, target, headers, body) = (
                login.clone(),
                url.to_string(),
                headers.clone(),
                form.clone(),
            );
            let response = tokio::task::spawn_blocking(move || {
                super::pkulogin::request_in_session(&session, &target, &headers, body.as_deref())
            })
            .await
            .map_err(|_| "教学网请求任务失败")?
            .map_err(|error| transport_error("连接教学网", &route, &error))?;
            // Only same-origin GET redirects may be followed. Never replay a grade POST.
            if form.is_none() && (300..400).contains(&response.status) {
                if let Some(location) = &response.location {
                    let next = url.join(location).map_err(|_| "教学网重定向地址无效")?;
                    if same_origin(&self.base, &next) && !next.path().contains("/webapps/login") {
                        url = self.url(next.as_str())?;
                        continue;
                    }
                }
            }
            return self.bytes(response, &url).await;
        }
        Err("教学网重定向过多".into())
    }
    pub(super) async fn get(&self, path: &str) -> Result<Vec<u8>, String> {
        self.cached_get(path, self.cached.as_ref().is_some_and(|c| c.fresh)).await
    }
    async fn json(&self, path: &str) -> Result<Value, String> {
        let route = safe_route(&self.url(path)?);
        serde_json::from_slice(&self.get(path).await?).map_err(|error| {
            tracing::warn!(target: "teaching_ta", route=%route, line=error.line(), column=error.column(), "教学网响应不是可识别的 JSON");
            "教学网返回了无法识别的数据，请刷新重试".into()
        })
    }
    async fn list(&self, path: &str) -> Result<Vec<Value>, String> {
        let mut next = path.to_string();
        let mut visited = HashSet::new();
        let mut results = Vec::new();
        for _ in 0..MAX_PAGES {
            let url = self.url(&next)?.to_string();
            if !visited.insert(url.clone()) {
                return Err("教学网分页出现循环，已停止加载".into());
            }
            let page = self.json(&url).await?;
            if let Some(items) = page.as_array() {
                results.extend(items.iter().cloned());
                return Ok(results);
            }
            let items = page
                .get("results")
                .and_then(Value::as_array)
                .ok_or("教学网列表格式不正确")?;
            results.extend(items.iter().cloned());
            if results.len() > 50_000 {
                return Err("教学网列表过大，已停止加载".into());
            }
            match page
                .pointer("/paging/nextPage")
                .or_else(|| page.get("nextPage"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                Some(path) => {
                    next = self
                        .url(&url)?
                        .join(path)
                        .map_err(|_| "教学网分页地址无效")?
                        .to_string();
                }
                None => return Ok(results),
            }
        }
        Err("教学网分页超过限制，未返回不完整列表".into())
    }

    pub async fn courses(&self) -> Result<Vec<Course>, String> {
        let page = self.get(COURSE_PORTAL).await?;
        let html = std::str::from_utf8(&page).map_err(|_| "教学网课程页面编码错误")?;
        if !super::pkulogin::is_teaching_portal(html) {
            return Err("教学网返回的内容不是可识别的课程页面".into());
        }
        course_parser::parse_ta_courses(html)
            .into_iter()
            .map(|course| {
                valid_id(&course.key)?;
                Ok(Course {
                    id: course.key,
                    name: course.name,
                    term: course.term,
                })
            })
            .collect()
    }
    pub async fn authorize(&self, course: &str) -> Result<Course, String> {
        valid_id(course)?;
        self.courses()
            .await?
            .into_iter()
            .find(|c| c.id == course)
            .ok_or("当前账号不是这门课程的助教".into())
    }
    pub async fn assignments(&self, course: &str) -> Result<Vec<Assignment>, String> {
        valid_id(course)?;
        self.list(&format!(
            "/learn/api/public/v2/courses/{course}/gradebook/columns?limit=100"
        ))
        .await?
        .into_iter()
        .filter(|v| v.pointer("/grading/type").and_then(Value::as_str) != Some("Calculated"))
        .map(|v| {
            Ok(Assignment {
                id: valid_id(string(&v, "id"))?.into(),
                name: string(&v, "name").into(),
                possible: v
                    .pointer("/score/possible")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
            })
        })
        .collect()
    }
    pub async fn assignment(&self, course: &str, column: &str) -> Result<Assignment, String> {
        valid_id(column)?;
        self.assignments(course)
            .await?
            .into_iter()
            .find(|a| a.id == column)
            .ok_or("未找到此作业".into())
    }
    pub async fn groups(&self, course: &str) -> Result<Vec<Group>, String> {
        valid_id(course)?;
        self.list(&format!(
            "/learn/api/public/v1/courses/{course}/groups?limit=100"
        ))
        .await?
        .into_iter()
        .filter(|v| v.get("isGroupSet").and_then(Value::as_bool) != Some(true))
        .map(|v| {
            Ok(Group {
                id: valid_id(string(&v, "id"))?.into(),
                name: string(&v, "name").into(),
            })
        })
        .collect()
    }
    pub async fn group_users(&self, course: &str, group: &str) -> Result<HashSet<String>, String> {
        valid_id(course)?;
        valid_id(group)?;
        Ok(self
            .list(&format!(
                "/learn/api/public/v2/courses/{course}/groups/{group}/users?limit=100"
            ))
            .await?
            .iter()
            .map(|v| string(v, "userId").to_string())
            .collect())
    }
    pub async fn students(&self, course: &str) -> Result<HashMap<String, Student>, String> {
        valid_id(course)?;
        let mut students = HashMap::new();
        for v in self
            .list(&format!(
                "/learn/api/public/v1/courses/{course}/users?expand=user&limit=100"
            ))
            .await?
        {
            let id = valid_id(string(&v, "userId"))?.to_string();
            let user = match v.get("user") {
                Some(user) if user.is_object() => user.clone(),
                _ => {
                    self.json(&format!("/learn/api/public/v1/users/{id}"))
                        .await?
                }
            };
            let given = user
                .pointer("/name/given")
                .and_then(Value::as_str)
                .unwrap_or("");
            let family = user
                .pointer("/name/family")
                .and_then(Value::as_str)
                .unwrap_or("");
            let name = format!("{family}{given}");
            students.insert(
                id.clone(),
                Student {
                    membership: valid_id(string(&v, "id"))?.into(),
                    number: user
                        .get("userName")
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .into(),
                    name: if name.is_empty() { id } else { name },
                },
            );
        }
        Ok(students)
    }
    pub async fn attempts(&self, course: &str, column: &str) -> Result<Vec<Attempt>, String> {
        valid_id(course)?;
        valid_id(column)?;
        self.list(&format!("/learn/api/public/v2/courses/{course}/gradebook/columns/{column}/attempts?limit=100")).await?
            .into_iter().map(|v| {
                let score = attempt_score(&v);
                let status = string(&v,"status").to_string();
                let date = string(&v,"attemptDate").to_string();
                let revision = format!("{:x}", Sha256::digest(serde_json::to_vec(&serde_json::json!({
                    "id":v["id"],"userId":v["userId"],"date":date,"status":status,
                    "score":v["score"],"gradeScore":v.pointer("/grade/score"),"exempt":v["exempt"]
                })).unwrap()));
                Ok(Attempt { id: valid_id(string(&v,"id"))?.into(), user_id: valid_id(string(&v,"userId"))?.into(),
                    date, status, score, revision, exempt: v.get("exempt").and_then(Value::as_bool).unwrap_or(false) })
            }).collect()
    }

    pub async fn submission(
        &self,
        course: &str,
        column: &str,
        attempt: &Attempt,
        student: &Student,
    ) -> Result<Submission, String> {
        for id in [course, column, &attempt.id, &student.membership] {
            valid_id(id)?;
        }
        let path = format!("/webapps/assignment/gradeAssignmentRedirector?outcomeDefinitionId={column}&course_id={course}&attempt_id={}&courseMembershipId={}&mode=inline&currentAttemptIndex=0&numAttempts=1&sequenceId={course}_0", attempt.id, student.membership);
        let fresh = self.cached.as_ref().is_some_and(|c| c.refresh_submissions);
        let html = String::from_utf8(self.cached_get_version(&path, fresh, &attempt.revision).await?).map_err(|_| "提交页面编码错误")?;
        let (text, file_ids) = parse_submission(&html);
        let mut files = Vec::new();
        for id in file_ids {
            let path = format!(
                "/webapps/assignment/inlineView?file_id={}&attempt_id={}&course_id={course}",
                urlencoding::encode(&id),
                attempt.id
            );
            let info: Value = serde_json::from_slice(&self.cached_get_version(&path, fresh, &attempt.revision).await?)
                .map_err(|_| "教学网附件信息格式不正确")?;
            let url = string(&info, "downloadUrl");
            if url.is_empty() {
                return Err("教学网未返回附件下载地址".into());
            }
            self.url(url)?;
            files.push(Attachment {
                id,
                name: string(&info, "fileName").to_string(),
                url: url.into(),
            });
        }
        Ok(Submission { text, files })
    }
    pub async fn download(&self, file: &Attachment) -> Result<Vec<u8>, String> {
        self.cached_get(&file.url, false).await
    }

    pub async fn save_grade(
        &self,
        course: &str,
        column: &str,
        attempt: &Attempt,
        score: f64,
        feedback: &str,
    ) -> Result<(), String> {
        for id in [course, column, &attempt.id] {
            valid_id(id)?;
        }
        let page = self
            .request(&format!(
                "/webapps/gradebook/controller/reconcileGrades?course_id={course}&id={column}"
            ), Vec::new(), None)
            .await?;
        let nonce = parse_nonce(&String::from_utf8_lossy(&page))?;
        let route = "/webapps/gradebook/controller/saveReconcileGrade";
        let fields = [
            ("attemptId", attempt.id.clone()),
            ("gradableItemId", column.into()),
            ("score", format!("{score:.2}")),
            ("course_id", course.into()),
            ("hasFeedback", "true".into()),
            ("myfeedbacktext", feedback.into()),
            ("showStagedFeedbackToStu", "true".into()),
            ("isDetailPage", "false".into()),
            ("reconcileMode", "A".into()),
            ("blackboard.platform.security.NonceUtil.nonce.ajax", nonce),
        ];
        let form = fields
            .iter()
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    urlencoding::encode(key),
                    urlencoding::encode(value)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let headers = vec![
            (
                "Origin".into(),
                self.base.as_str().trim_end_matches('/').into(),
            ),
            (
                "Referer".into(),
                format!(
                    "{}webapps/gradebook/controller/reconcileGrades?course_id={course}&id={column}",
                    self.base
                ),
            ),
            (
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
            ("X-Requested-With".into(), "XMLHttpRequest".into()),
            ("X-Prototype-Version".into(), "1.7".into()),
        ];
        let body = self
            .request(route, headers, Some(form))
            .await
            .map_err(|error| format!("{error}；登分结果尚未确认，请先核对成绩，勿直接重试"))?;
        if let Ok(v) = serde_json::from_slice::<Value>(&body) {
            if v.get("success").and_then(Value::as_bool) == Some(false)
                || v.get("error").is_some_and(|e| !e.is_null() && e != false)
            {
                return Err("教学网未接受评分，请刷新核对成绩".into());
            }
        }
        // Never treat HTTP 200 as a successful grade write. Re-read the exact attempt.
        let data: Value = serde_json::from_slice(&self
            .request(&format!(
                "/webapps/gradebook/controller/loadReconcileData?course_id={course}&id={column}"
            ), Vec::new(), None)
            .await?).map_err(|_| "评分请求已发送，但回读数据无法识别；请在教学网核对后再操作")?;
        let actual = data
            .get("attempts")
            .and_then(Value::as_array)
            .and_then(|items| items.iter().find(|v| string(v, "attemptId") == attempt.id))
            .and_then(|v| v.get("reconciledScore"))
            .and_then(Value::as_f64);
        if actual.is_some_and(|actual| (actual - score).abs() < 0.005) {
            Ok(())
        } else {
            Err("评分请求已发送，但回读未确认目标分数；请在教学网核对后再操作".into())
        }
    }
}

/// Log route templates, never query strings, signed attachment paths or student IDs.
fn safe_route(url: &Url) -> String {
    let path = url.path();
    if path.starts_with("/learn/api/public/") {
        return path
            .split('/')
            .map(|part| {
                if part.starts_with('_')
                    || part.bytes().any(|c| c.is_ascii_digit())
                        && !matches!(part, "v1" | "v2" | "v3")
                {
                    ":id"
                } else {
                    part
                }
            })
            .collect::<Vec<_>>()
            .join("/");
    }
    if path.starts_with("/webapps/gradebook/controller/")
        || matches!(
            path,
            "/webapps/assignment/gradeAssignmentRedirector" | "/webapps/assignment/inlineView"
        )
    {
        return path.into();
    }
    "/[attachment-or-redirect]".into()
}

fn transport_error(stage: &str, route: &str, error: &str) -> String {
    let detail = redact_transport_detail(error);
    tracing::warn!(target: "teaching_ta", stage, route, cause=%detail, "助教教学网 curl 请求失败");
    format!("{stage}失败：{detail}，请查看后台 teaching_ta 日志")
}

fn redact_transport_detail(detail: &str) -> String {
    // Defensive redaction for nested transport errors that might carry a proxy/redirect URL.
    regex::Regex::new(r#"https?://[^\s\"<>]+"#)
        .unwrap()
        .replace_all(detail, "[URL已隐藏]")
        .into_owned()
}

fn same_origin(base: &Url, url: &Url) -> bool {
    base.scheme() == url.scheme()
        && base.host_str() == url.host_str()
        && base.port_or_known_default() == url.port_or_known_default()
}
pub fn valid_id(id: &str) -> Result<&str, String> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        Err("教学网标识格式无效".into())
    } else {
        Ok(id)
    }
}
fn string<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn parse_nonce(html: &str) -> Result<String, String> {
    let dom = scraper::Html::parse_document(html);
    let selector =
        scraper::Selector::parse("input[name='blackboard.platform.security.NonceUtil.nonce.ajax']")
            .unwrap();
    dom.select(&selector)
        .find_map(|e| e.value().attr("value"))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or("无法取得教学网评分凭据，当前作业可能不支持此评分方式".into())
}
fn parse_submission(html: &str) -> (String, Vec<String>) {
    let dom = scraper::Html::parse_document(html);
    let selector =
        scraper::Selector::parse("#submissionText, #submissionTextView, .submissionText").unwrap();
    let text = dom
        .select(&selector)
        .map(|e| e.text().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n");
    let re = regex::Regex::new(r#"inlineView\(\s*event\s*,\s*['\"]([^'\"]+)['\"]"#).unwrap();
    let mut seen = HashSet::new();
    let ids = re
        .captures_iter(html)
        .map(|c| html_escape::decode_html_entities(&c[1]).into_owned())
        .filter(|id| seen.insert(id.clone()))
        .collect();
    (text, ids)
}

pub fn validate_score(score: f64, possible: f64) -> Result<f64, String> {
    if !possible.is_finite() || possible <= 0.0 {
        return Err("作业满分无效，请先在教学网设置满分".into());
    }
    if !score.is_finite() || score < 0.0 || score > possible {
        return Err(format!("分数须在 0 到 {possible} 之间"));
    }
    if (score * 100.0 - (score * 100.0).round()).abs() > 0.000_001 {
        return Err("分数最多保留两位小数".into());
    }
    Ok(score)
}

/// The default view chooses one attempt per student; history is always explicit.
pub fn latest_attempts(attempts: &[Attempt]) -> HashSet<String> {
    let mut latest: HashMap<&str, &Attempt> = HashMap::new();
    for attempt in attempts {
        let previous = latest.entry(&attempt.user_id).or_insert(attempt);
        if (&attempt.date, &attempt.id) > (&previous.date, &previous.id) {
            *previous = attempt;
        }
    }
    latest.values().map(|a| a.id.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_zero_is_not_a_grade_but_completed_zero_is() {
        for status in [
            "NeedsGrading",
            "Needs Grading",
            "InProgress",
            "Submitted",
            "",
        ] {
            let value = serde_json::json!({"status":status,"score":0});
            assert_eq!(attempt_score(&value), None, "{status}");
            assert!(!attempt_is_graded(status, Some(0.0)), "{status}");
        }
        for status in ["Completed", "Graded"] {
            assert_eq!(
                attempt_score(&serde_json::json!({"status":status,"score":0})),
                Some(0.0)
            );
            assert!(attempt_is_graded(status, Some(0.0)));
        }
        assert_eq!(
            attempt_score(&serde_json::json!({"status":"NeedsGrading","grade":{"score":0}})),
            None
        );
        assert_eq!(
            attempt_score(&serde_json::json!({"status":"Completed","grade":{"score":85}})),
            Some(85.0)
        );
        assert!(!attempt_is_graded("NeedsGrading", Some(85.0)));
    }

    #[tokio::test]
    async fn curl_reuses_cookie_jar_and_never_replays_post_or_external_redirect() {
        use axum::{
            extract::State,
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let reads = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/start",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [
                            ("location", "/end"),
                            ("set-cookie", "ta_session=shared; Path=/; HttpOnly"),
                        ],
                    )
                }),
            )
            .route(
                "/end",
                get(
                    |State(reads): State<Arc<AtomicUsize>>, headers: HeaderMap| async move {
                        assert!(headers["cookie"]
                            .to_str()
                            .unwrap()
                            .contains("ta_session=shared"));
                        assert_eq!(headers["user-agent"], "Windows TA-test");
                        reads.fetch_add(1, Ordering::SeqCst);
                        "ok"
                    },
                ),
            )
            .route(
                "/post",
                axum::routing::post(|| async {
                    (StatusCode::TEMPORARY_REDIRECT, [("location", "/end")])
                }),
            )
            .route(
                "/external",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [("location", "https://external.invalid/secret")],
                    )
                }),
            )
            .route("/denied", get(|| async { StatusCode::FORBIDDEN }))
            .route("/expired", get(|| async { StatusCode::UNAUTHORIZED }))
            .with_state(reads.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Blackboard::test_client(&format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        assert_eq!(client.get("/start").await.unwrap(), b"ok");
        assert!(
            std::fs::read_to_string(&client.login.as_ref().unwrap().cookie_file)
                .unwrap()
                .contains("ta_session")
        );
        assert!(client
            .request("/post", Vec::new(), Some("score=10".into()))
            .await
            .unwrap_err()
            .contains("307"));
        assert!(client.get("/external").await.unwrap_err().contains("302"));
        assert!(client
            .get("/denied")
            .await
            .unwrap_err()
            .contains("助教权限"));
        assert!(client
            .get("/expired")
            .await
            .unwrap_err()
            .contains("登录已失效"));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[test]
    fn diagnostics_hide_query_strings_and_dynamic_paths() {
        let url = Url::parse("https://course.pku.edu.cn/learn/api/public/v1/courses/_123_1/users/_456_1?token=secret").unwrap();
        let route = safe_route(&url);
        assert_eq!(route, "/learn/api/public/v1/courses/:id/users/:id");
        assert!(!route.contains("secret"));
        let file =
            Url::parse("https://course.pku.edu.cn/bbcswebdav/student-name.pdf?signature=secret")
                .unwrap();
        assert_eq!(safe_route(&file), "/[attachment-or-redirect]");
        let detail = redact_transport_detail(
            "Connect failed: https://user:password@proxy.test/path?token=secret",
        );
        assert!(!detail.contains("password") && !detail.contains("secret"));
    }

    /// Uses the same curl runner as replay, without credentials or grade writes.
    #[tokio::test]
    #[ignore]
    async fn teaching_curl_smoke_test() {
        let client = Blackboard::test_client(BASE);
        let login = client.login.unwrap();
        let response = tokio::task::spawn_blocking(move || {
            super::super::pkulogin::request_in_session(
                &login,
                &format!("{BASE}/learn/api/public/v1/users/me"),
                &[],
                None,
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status, 401);
        println!("Replay curl: HTTP 401 (no credentials sent)");
    }
    #[test]
    fn rejects_invalid_scores_and_ids() {
        for score in [f64::NAN, f64::INFINITY, -1.0, 101.0, 1.234] {
            assert!(validate_score(score, 100.0).is_err());
        }
        assert_eq!(validate_score(0.0, 100.0).unwrap(), 0.0);
        assert_eq!(validate_score(99.99, 100.0).unwrap(), 99.99);
        assert!(validate_score(1.0, 0.0).is_err());
        for id in ["", "../me", "x?role=TA", "x/y"] {
            assert!(valid_id(id).is_err());
        }
    }
    #[test]
    fn parses_all_files_deduplicates_and_escapes_text() {
        let (text, ids) = parse_submission("<div id='submissionText'>回答 &lt;script&gt;</div><a onclick=\"inlineView(event, '_1_1')\"></a><a onclick=\"inlineView(event, '_2_1')\"></a><a onclick=\"inlineView(event, '_1_1')\"></a>");
        assert!(text.contains("回答 <script>"));
        assert_eq!(ids, vec!["_1_1", "_2_1"]);
        assert_eq!(parse_nonce("<input value='abc&amp;def' name='blackboard.platform.security.NonceUtil.nonce.ajax'>").unwrap(),"abc&def");
    }
    #[test]
    fn origin_check_rejects_external_and_downgraded_addresses() {
        let base = Url::parse(BASE).unwrap();
        for url in [
            "http://course.pku.edu.cn/x",
            "https://course.pku.edu.cn.evil.test/x",
            "https://iaaa.pku.edu.cn/x",
            "https://course.pku.edu.cn:444/x",
        ] {
            assert!(!same_origin(&base, &Url::parse(url).unwrap()));
        }
    }
    #[tokio::test]
    async fn persistent_reads_only_refetch_on_refresh_and_keep_cache_on_failure() {
        use axum::{routing::get, Router};
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let app = Router::new().route("/resource", get(move || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n >= 2 { (axum::http::StatusCode::SERVICE_UNAVAILABLE, vec![]) }
                else { (axum::http::StatusCode::OK, vec![0, 255, n as u8]) }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let tmp = tempdir::TempDir::new("ta-cache-test").unwrap();
        let settings: crate::config::Settings = serde_json::from_value(serde_json::json!({
            "server":{"address":"127.0.0.1","port":0},
            "wechat":{"api_base_url":"","oauth_app_id":"","oauth_app_secret":"","redirect_uri":"","qr_bot_type":""},
            "security":{"session_ttl_hours":1},"cors":{"allowed_origins":[],"allow_credentials":false},
            "audioapi_paraformer":{},"llm_deepseek":{}
        })).unwrap();
        let state = crate::app_state::AppState::new(settings.clone(), Arc::new(crate::store::FileStore::new(tmp.path().into()).await.unwrap()));
        let user = crate::auth::CurrentUser { id: uuid::Uuid::new_v4(), username: "test".into(), email: "test@pku.edu.cn".into() };
        let mut bb = Blackboard::test_client(&base);
        bb.cached = Blackboard::cached(&state, &user, false).cached;
        assert_eq!(bb.get("/resource").await.unwrap(), vec![0, 255, 0]);
        assert_eq!(bb.get("/resource").await.unwrap(), vec![0, 255, 0]);
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // A new store/client can read the persisted bytes without any teaching credentials.
        let restarted = crate::app_state::AppState::new(settings, Arc::new(crate::store::FileStore::new(tmp.path().into()).await.unwrap()));
        let mut offline = Blackboard::cached(&restarted, &user, false);
        offline.base = Url::parse(&base).unwrap();
        assert_eq!(offline.get("/resource").await.unwrap(), vec![0, 255, 0]);
        assert!(offline.cached.as_ref().unwrap().login.get().is_none());
        let other = crate::auth::CurrentUser { id: uuid::Uuid::new_v4(), ..user.clone() };
        let mut isolated = Blackboard::cached(&restarted, &other, false);
        isolated.base = offline.base.clone();
        assert_ne!(offline.cache_path("/resource").unwrap(), isolated.cache_path("/resource").unwrap());
        assert!(isolated.get("/resource").await.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);

        bb.cached.as_mut().unwrap().fresh = true;
        assert_eq!(bb.get("/resource").await.unwrap(), vec![0, 255, 1]);
        assert!(bb.get("/resource").await.is_err());
        assert_eq!(offline.get("/resource").await.unwrap(), vec![0, 255, 1]);
        // Even a live validation client reuses downloaded immutable attachment bytes.
        let file = Attachment { id: "file".into(), name: "test.bin".into(), url: format!("{base}/resource") };
        assert_eq!(bb.download(&file).await.unwrap(), vec![0, 255, 1]);
        assert_eq!(count.load(Ordering::SeqCst), 3);
        assert!(offline.get("https://evil.test/leak").await.is_err());
        server.abort();
    }

    #[tokio::test]
    async fn reads_ta_courses_from_the_portal_and_follows_api_pagination() {
        use axum::{routing::get, Json, Router};
        let app = Router::new()
            .route("/webapps/portal/execute/tabs/tabAction", get(|| async {
                axum::response::Html(r#"
                    <a href="/webapps/blackboard/execute/launcher?type=Course&amp;id=PkId{key=_student_1, dataType=blackboard.data.course.Course}&amp;url=">编号: 学生课程(26-27学年第1学期)</a>
                    <a href=" /webapps/blackboard/execute/launcher?type=Course&amp;id=PkId{key=_104104_1, dataType=blackboard.data.course.Course, container=blackboard.persist.DatabaseContainer@2c7c68e8}&amp;url=" target="_top">26271-00048-04833040-0006161023-00-1: 计算机系统导论(26-27学年第1学期)</a>
                "#)
            }))
            .route("/first", get(|| async {
                Json(serde_json::json!({"results":[1],"paging":{"nextPage":"/second"}}))
            }))
            .route("/second", get(|| async { Json(serde_json::json!({"results":[2]})) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let bb = Blackboard::test_client(base.as_str());
        let courses = bb.courses().await.unwrap();
        assert_eq!(courses.len(), 1);
        assert_eq!(courses[0].id, "_104104_1");
        assert_eq!(courses[0].name, "计算机系统导论");
        assert_eq!(courses[0].term, "26-27学年第1学期");
        assert!(bb.authorize("_student_1").await.is_err());
        assert_eq!(bb.list("/first").await.unwrap(), vec![Value::from(1), Value::from(2)]);
        assert!(bb.list("https://evil.test/leak").await.is_err());
        server.abort();
    }
}
