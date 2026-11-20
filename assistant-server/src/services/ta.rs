//! Native Blackboard TA client. Authentication is owned by the existing teaching login cache.
use super::{course_parser, pkulogin::PkuLoginResult};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

const BASE: &str = "https://course.pku.edu.cn";
const COURSE_PORTAL: &str = "/webapps/portal/execute/tabs/tabAction?tab_tab_group_id=_1_1";
const NEEDS_GRADING: &str = "/webapps/gradebook/do/instructor/viewNeedsGrading";
const MAX_PAGES: usize = 100;

pub(crate) fn course_portal_cache_pattern() -> String {
    let hash = Sha256::digest(format!("{BASE}{COURSE_PORTAL}"));
    format!("sidecar/ta_cache/*/{hash:x}.json")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Course {
    pub id: String,
    pub name: String,
    pub term: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Assignment {
    pub id: String,
    pub name: String,
    pub attempts: Vec<Attempt>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    pub id: String,
    pub student_name: String,
    pub date: String,
    pub revision: String,
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
    #[serde(default, alias = "student_id")]
    pub student_name: String,
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

    fn response_cache_path(&self, path: &str, version: &str) -> Result<std::path::PathBuf, String> {
        let mut key = self.cache_path(path)?;
        if !version.is_empty() {
            key.set_extension(format!("{:x}.json", Sha256::digest(version)));
        }
        Ok(key)
    }

    async fn stored_response(&self, key: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let Some(cache) = &self.cached else { return Ok(None); };
        cache.state.store.load_sidecar::<CachedResponse>(key).await?
            .map(|saved| STANDARD.decode(saved.body).map_err(|_| "教学网本地缓存损坏，请点击刷新".into()))
            .transpose()
    }

    async fn cached_get_version(&self, path: &str, fresh: bool, version: &str) -> Result<Vec<u8>, String> {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let Some(cache) = &self.cached else { return self.request(path, Vec::new(), None).await; };
        let key = self.response_cache_path(path, version)?;
        if !fresh {
            if let Some(saved) = self.stored_response(&key).await? {
                return Ok(saved);
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
    pub async fn courses(&self) -> Result<Vec<Course>, String> {
        let Some(cache) = &self.cached else { return self.refresh_courses().await; };
        let key = self.cache_path(COURSE_PORTAL)?;
        Ok(cache.state.store.load_sidecar(&key).await?.unwrap_or_default())
    }

    pub async fn refresh_courses(&self) -> Result<Vec<Course>, String> {
        let page = self.request(COURSE_PORTAL, Vec::new(), None).await?;
        let courses = Self::parse_courses(&page)?;
        if let Some(cache) = &self.cached {
            let payload = serde_json::to_string(&courses).map_err(|error| error.to_string())?;
            cache.state.store.save_sidecar(&self.cache_path(COURSE_PORTAL)?, &payload).await?;
        }
        tracing::info!(target: "teaching_ta", count=courses.len(), "已拉取助教课程列表");
        Ok(courses)
    }

    fn parse_courses(page: &[u8]) -> Result<Vec<Course>, String> {
        let html = std::str::from_utf8(page).map_err(|_| "教学网课程页面编码错误")?;
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
    pub async fn stored_grading_queue(&self, course: &str) -> Result<Option<Vec<Assignment>>, String> {
        valid_id(course)?;
        let Some(cache) = &self.cached else { return Ok(None); };
        let path = format!("{NEEDS_GRADING}?course_id={course}");
        cache.state.store.load_sidecar(&self.cache_path(&path)?).await
    }

    pub async fn grading_queue(&self, course: &str) -> Result<Vec<Assignment>, String> {
        valid_id(course)?;
        if self.cached.as_ref().is_some_and(|cache| !cache.fresh) {
            return Ok(self.stored_grading_queue(course).await?.unwrap_or_default());
        }
        let first = format!("{NEEDS_GRADING}?course_id={course}");
        let mut next = self.url(&first)?;
        let mut visited = HashSet::new();
        let mut attempts = HashSet::new();
        let mut assignments: Vec<Assignment> = Vec::new();
        let mut columns = HashMap::new();
        for _ in 0..MAX_PAGES {
            if !visited.insert(next.clone()) {
                return Err("教学网待评分列表分页出现循环".into());
            }
            let body = self.request(next.as_str(), Vec::new(), None).await?;
            let html = std::str::from_utf8(&body).map_err(|_| "教学网待评分列表编码错误")?;
            let (rows, next_page) = parse_grading_page(html, course)?;
            for (column, name, attempt) in rows {
                if !attempts.insert(attempt.id.clone()) {
                    continue;
                }
                let index = *columns.entry(column.clone()).or_insert_with(|| {
                    let index = assignments.len();
                    assignments.push(Assignment { id: column, name, attempts: Vec::new() });
                    index
                });
                assignments[index].attempts.push(attempt);
            }
            let Some(href) = next_page else {
                if let Some(cache) = &self.cached {
                    let payload = serde_json::to_string(&assignments).map_err(|error| error.to_string())?;
                    cache.state.store.save_sidecar(&self.cache_path(&first)?, &payload).await?;
                }
                return Ok(assignments);
            };
            next = next.join(&href).map_err(|_| "教学网分页地址无效")?;
            self.url(next.as_str())?;
        }
        Err("教学网待评分列表分页超过限制".into())
    }

    pub async fn stored_submission(&self, course: &str, attempt: &Attempt) -> Result<Option<Submission>, String> {
        self.read_submission(course, attempt, true).await
    }

    pub async fn submission(&self, course: &str, attempt: &Attempt) -> Result<Submission, String> {
        self.read_submission(course, attempt, false).await?.ok_or("无法读取教学网提交".into())
    }

    async fn submission_resource(&self, path: &str, version: &str, stored_only: bool) -> Result<Option<Vec<u8>>, String> {
        if stored_only {
            return self.stored_response(&self.response_cache_path(path, version)?).await;
        }
        let fresh = self.cached.as_ref().is_some_and(|c| c.refresh_submissions);
        self.cached_get_version(path, fresh, version).await.map(Some)
    }

    async fn read_submission(&self, course: &str, attempt: &Attempt, stored_only: bool) -> Result<Option<Submission>, String> {
        for id in [course, &attempt.id] {
            valid_id(id)?;
        }
        let path = format!("/webapps/assignment/gradeAssignmentRedirector?course_id={course}&attempt_id={}", attempt.id);
        let Some(body) = self.submission_resource(&path, &attempt.revision, stored_only).await? else {
            return Ok(None);
        };
        let html = String::from_utf8(body).map_err(|_| "提交页面编码错误")?;
        let (text, file_ids) = parse_submission(&html);
        let mut files = Vec::new();
        for id in file_ids {
            let path = format!(
                "/webapps/assignment/inlineView?file_id={}&attempt_id={}&course_id={course}",
                urlencoding::encode(&id),
                attempt.id
            );
            let Some(body) = self.submission_resource(&path, &attempt.revision, stored_only).await? else {
                return Ok(None);
            };
            let info: Value = serde_json::from_slice(&body).map_err(|_| "教学网附件信息格式不正确")?;
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
        Ok(Some(Submission { text, files }))
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
            "/webapps/portal/execute/tabs/tabAction"
                | NEEDS_GRADING
                | "/webapps/assignment/gradeAssignmentRedirector"
                | "/webapps/assignment/inlineView"
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

fn parse_grading_page(html: &str, course: &str) -> Result<(Vec<(String, String, Attempt)>, Option<String>), String> {
    use scraper::{Html, Selector};
    let dom = Html::parse_document(html);
    let rows = Selector::parse("tr").unwrap();
    let values = Selector::parse("td > .table-data-cell-value").unwrap();
    let menu = Selector::parse(".contextMenuContainer").unwrap();
    let link = Selector::parse("a.gradeAttempt[attemptid]").unwrap();
    let next = Selector::parse("#listContainer_nextpage_bot[href]").unwrap();
    let text = |element: scraper::ElementRef<'_>| {
        element.text().collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let mut attempts = Vec::new();
    for row in dom.select(&rows) {
        let mut cells = row.select(&values);
        let Some(category) = cells.next() else { continue };
        if text(category) != "作业" {
            continue;
        }
        let (Some(item), Some(link)) = (cells.next(), row.select(&link).next()) else { continue };
        let id = link.value().attr("attemptid").unwrap_or_default().trim();
        if id.is_empty() {
            continue;
        }
        let params = item.select(&menu).find_map(|menu| menu.value().attr("bb:contextparameters"))
            .ok_or("教学网待评分记录缺少作业信息")?;
        let params = Url::parse(&format!("https://course.pku.edu.cn/?{params}"))
            .map_err(|_| "教学网作业参数无效")?;
        let mut column = None;
        for (key, value) in params.query_pairs() {
            match key.as_ref() {
                "itemId" => column = Some(value.into_owned()),
                "course_id" if value != course => return Err("教学网待评分记录的课程不匹配".into()),
                _ => {}
            }
        }
        let column = column.ok_or("教学网待评分记录缺少作业 ID")?;
        valid_id(&column)?;
        valid_id(id)?;
        let date = cells.next().map(text).unwrap_or_default();
        let revision = format!("{:x}", Sha256::digest(format!("{course}:{column}:{id}:{date}")));
        let student_name = text(link);
        attempts.push((column, text(item), Attempt {
            id: id.into(),
            student_name: if student_name.is_empty() { id.into() } else { student_name },
            date,
            revision,
        }));
    }
    let href = dom.select(&next).find_map(|link| link.value().attr("href"))
        .map(str::trim).filter(|href| !href.is_empty() && *href != "#").map(str::to_owned);
    Ok((attempts, href))
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

#[cfg(test)]
pub(crate) fn grading_row(course: &str, column: &str, name: &str, attempt: &str, date: &str) -> String {
    format!(r##"<tr id="listContainer_row:10">
        <td><span class="mobile-table-label">类别: </span><span class="table-data-cell-value">作业</span></td>
        <td><span class="mobile-table-label">项目名称: </span><span class="table-data-cell-value">{name}
        <span class="contextMenuContainer" bb:contextparameters="trueAnonymousGrading=false&amp;itemId={column}&amp;course_id={course}&amp;scoreProviderHandle=resource/x-bb-assignment"><a title="操作"></a></span></span></td>
        <th scope="row"><a class="gradeAttempt" attemptid="{attempt}" groupattemptid="" href="#"> 测试院系 学生 </a></th>
        <td><span class="mobile-table-label">提交日期: </span><span class="table-data-cell-value">{date}</span></td>
        <td><span class="mobile-table-label">到期日期: </span><span class="table-data-cell-value"></span></td>
    </tr>"##)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let url = Url::parse("https://course.pku.edu.cn/webapps/gradebook/do/instructor/viewNeedsGrading?course_id=_123_1&token=secret").unwrap();
        let route = safe_route(&url);
        assert_eq!(route, NEEDS_GRADING);
        assert!(!route.contains("_123_1"));
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
        let portal_count = Arc::new(AtomicUsize::new(0));
        let portal_calls = portal_count.clone();
        let queue_count = Arc::new(AtomicUsize::new(0));
        let queue_calls = queue_count.clone();
        let submission_count = Arc::new(AtomicUsize::new(0));
        let submission_calls = submission_count.clone();
        let app = Router::new().route("/resource", get(move || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n >= 2 { (axum::http::StatusCode::SERVICE_UNAVAILABLE, vec![]) }
                else { (axum::http::StatusCode::OK, vec![0, 255, n as u8]) }
            }
        }))
        .route("/webapps/portal/execute/tabs/tabAction", get(move || {
            let n = portal_calls.fetch_add(1, Ordering::SeqCst);
            async move {
                axum::response::Html(if n == 0 {
                    r#"<a href=" /webapps/blackboard/execute/launcher?type=Course&amp;id=PkId{key=_ta_1, dataType=blackboard.data.course.Course}&amp;url=">编号: 助教课程</a>"#
                } else {
                    "<html>服务暂不可用</html>"
                })
            }
        }))
        .route(NEEDS_GRADING, get(move || {
            queue_calls.fetch_add(1, Ordering::SeqCst);
            async {
                let row = grading_row("_ta_1", "_a_1", "作业一", "_s_1", "2026年11月11日");
                axum::response::Html(format!("<table>{row}</table>"))
            }
        }))
        .route("/webapps/assignment/gradeAssignmentRedirector", get(move || {
            submission_calls.fetch_add(1, Ordering::SeqCst);
            async { axum::response::Html("<div id='submissionText'>已提交的正文</div>") }
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
        assert!(bb.courses().await.unwrap().is_empty());
        assert_eq!(portal_count.load(Ordering::SeqCst), 0);
        assert_eq!(bb.refresh_courses().await.unwrap()[0].id, "_ta_1");
        let key = bb.cache_path(COURSE_PORTAL).unwrap();
        let payload = state.store.stored_sidecar(&key).await.unwrap().unwrap();
        assert_eq!(serde_json::from_str::<Value>(&payload).unwrap(),
            serde_json::json!([{"id":"_ta_1","name":"助教课程","term":""}]));
        assert_eq!(bb.courses().await.unwrap()[0].id, "_ta_1");
        assert_eq!(portal_count.load(Ordering::SeqCst), 1);
        assert_eq!(bb.get("/resource").await.unwrap(), vec![0, 255, 0]);
        assert_eq!(bb.get("/resource").await.unwrap(), vec![0, 255, 0]);
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // A new store/client can read the persisted bytes without any teaching credentials.
        let legacy_key = Blackboard::cached(&state, &user, false).cache_path(COURSE_PORTAL).unwrap();
        let other = crate::auth::CurrentUser { id: uuid::Uuid::new_v4(), ..user.clone() };
        let parsed_key = Blackboard::cached(&state, &other, false).cache_path(COURSE_PORTAL).unwrap();
        state.store.save_sidecar(&legacy_key, r#"{"body":"旧门户缓存"}"#).await.unwrap();
        state.store.save_sidecar(&parsed_key, &payload).await.unwrap();
        let restarted = crate::app_state::AppState::new(settings, Arc::new(crate::store::FileStore::new(tmp.path().into()).await.unwrap()));
        assert_eq!(restarted.store.stored_sidecar(&legacy_key).await.unwrap(), None);
        assert_eq!(restarted.store.stored_sidecar(&parsed_key).await.unwrap(), Some(payload));
        assert!(Blackboard::cached(&restarted, &user, false).courses().await.unwrap().is_empty());
        let mut offline = Blackboard::cached(&restarted, &user, false);
        offline.base = Url::parse(&base).unwrap();
        assert_eq!(offline.courses().await.unwrap()[0].id, "_ta_1");
        assert_eq!(offline.get("/resource").await.unwrap(), vec![0, 255, 0]);
        assert!(offline.cached.as_ref().unwrap().login.get().is_none());
        let mut isolated = Blackboard::cached(&restarted, &other, false);
        isolated.base = offline.base.clone();
        assert_ne!(offline.cache_path("/resource").unwrap(), isolated.cache_path("/resource").unwrap());
        assert!(isolated.get("/resource").await.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);

        bb.cached.as_mut().unwrap().fresh = true;
        assert_eq!(bb.authorize("_ta_1").await.unwrap().id, "_ta_1");
        assert_eq!(portal_count.load(Ordering::SeqCst), 1);
        assert!(bb.refresh_courses().await.is_err());
        assert_eq!(offline.courses().await.unwrap()[0].id, "_ta_1");
        assert_eq!(portal_count.load(Ordering::SeqCst), 2);
        assert!(offline.stored_grading_queue("_ta_1").await.unwrap().is_none());
        assert!(offline.grading_queue("_ta_1").await.unwrap().is_empty());
        assert_eq!(queue_count.load(Ordering::SeqCst), 0);
        let queue = bb.grading_queue("_ta_1").await.unwrap();
        assert_eq!(queue_count.load(Ordering::SeqCst), 1);
        assert_eq!(offline.stored_grading_queue("_ta_1").await.unwrap().unwrap()[0].id, "_a_1");
        assert_eq!(queue_count.load(Ordering::SeqCst), 1);
        let attempt = &queue[0].attempts[0];
        assert!(offline.stored_submission("_ta_1", attempt).await.unwrap().is_none());
        assert_eq!(submission_count.load(Ordering::SeqCst), 0);
        assert_eq!(bb.submission("_ta_1", attempt).await.unwrap().text, "已提交的正文");
        assert_eq!(offline.stored_submission("_ta_1", attempt).await.unwrap().unwrap().text, "已提交的正文");
        assert_eq!(submission_count.load(Ordering::SeqCst), 1);
        assert!(offline.cached.as_ref().unwrap().login.get().is_none());

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
    async fn reads_ta_courses_and_paginates_pending_assignment_rows() {
        use axum::{extract::Query, response::Html, routing::get, Router};
        let app = Router::new()
            .route("/webapps/portal/execute/tabs/tabAction", get(|| async {
                axum::response::Html(r#"
                    <a href="/webapps/blackboard/execute/launcher?type=Course&amp;id=PkId{key=_student_1, dataType=blackboard.data.course.Course}&amp;url=">编号: 学生课程(26-27学年第1学期)</a>
                    <a href=" /webapps/blackboard/execute/launcher?type=Course&amp;id=PkId{key=_104104_1, dataType=blackboard.data.course.Course, container=blackboard.persist.DatabaseContainer@2c7c68e8}&amp;url=" target="_top">26271-00048-04833040-0006161023-00-1: 计算机系统导论(26-27学年第1学期)</a>
                "#)
            }))
            .route(NEEDS_GRADING, get(|Query(query): Query<HashMap<String, String>>| async move {
                let course = &query["course_id"];
                let first = grading_row(course, "_h_1", "作业一", "_a_1", "2026年11月11日 21:38:02");
                let second = grading_row(course, "_h_1", "作业一", "_a_2", "2026年11月12日");
                let other = grading_row(course, "_h_2", "作业二", "_a_3", "2026年11月12日");
                let test = grading_row(course, "_quiz_1", "测试", "_quiz_attempt", "")
                    .replace(">作业</span>", ">测试</span>");
                let body = match query.get("page").map(String::as_str) {
                    Some("2") => format!("<table>{first}{second}{other}</table>"),
                    _ => format!(r#"<table>{first}{test}</table><a id="listContainer_nextpage_bot" href="?course_id={course}&amp;page=2">下一页</a>"#),
                };
                Html(body)
            }));
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
        let queue = bb.grading_queue("_104104_1").await.unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].name, "作业一");
        assert_eq!(queue[0].attempts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["_a_1", "_a_2"]);
        assert_eq!(queue[0].attempts[0].student_name, "测试院系 学生");
        assert_eq!(queue[0].attempts[0].date, "2026年11月11日 21:38:02");
        assert_eq!(queue[1].attempts[0].id, "_a_3");
        server.abort();
    }
}
