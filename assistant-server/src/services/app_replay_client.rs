use super::{app_replay::ManualReplay, app_replay_auth::AppAuth};
use md5::{Digest, Md5};
use reqwest::{Client, Url};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    time::Duration,
};

pub struct AppReplayClient {
    auth: AppAuth,
    api_client: Client,
    user_agent: String,
    user: Option<Value>,
    user_id: Option<uuid::Uuid>,
    invalid_session: std::sync::atomic::AtomicBool,
    bound_user: Option<crate::store::FileUser>,
}

impl AppReplayClient {
    pub async fn start() -> Result<Self, String> {
        Self::with_user_agent(super::user_agents::choose_app("./user_agents.txt.gz")?)
    }
    pub async fn for_user(user_id: uuid::Uuid) -> Result<Self, String> {
        let saved = super::app_replay_vault::load::<Value>(user_id, "session")?;
        let saved_ua=saved.as_ref().and_then(|session|session["user_agent"].as_str());
        let reusable=match saved_ua {Some(ua)=>super::user_agents::valid_header(ua),None=>false};
        let ua=if reusable {saved_ua.unwrap().to_string()} else {super::user_agents::choose_app("./user_agents.txt.gz")?};
        let mut client = Self::with_user_agent(ua)?;
        client.user_id = Some(user_id);
        if let Some(saved) = saved.filter(|_|reusable) {
            if saved["user"]["_token"]
                .as_str()
                .is_some_and(|s| !s.is_empty() && !s.contains(['\r', '\n']))
            {
                client.user = Some(saved["user"].clone());
                tracing::info!(target:"app_replay_auth",%user_id,"restored persistent App login");
            }
        }
        Ok(client)
    }
    fn persist_session(&self) -> Result<(), String> {
        if let Some(user_id) = self.user_id {
            super::app_replay_vault::save(
                user_id,
                "session",
                &json!({"user":self.user,"user_agent":self.user_agent}),
            )?;
        }
        Ok(())
    }
    pub async fn login_bound(
        &mut self,
        user: &crate::store::FileUser,
        challenge: &Value,
    ) -> Result<Value, String> {
        self.bound_user = Some(user.clone());
        if self.user.is_some()
            && !self
                .invalid_session
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(json!({"ok":true,"data":{"logged_in":true}}));
        }
        let credential = super::app_replay_vault::load::<Value>(user.id, "binding")?;
        let mut request = json!({"action":"login","account":user.email.split('@').next().unwrap_or_default(),
            "encrypted_password":user.iaaa_encrypted_password,"stored_public_key":user.iaaa_public_key});
        if let Some(credential) = credential {
            request["account"] = credential["account"].clone();
            request["password"] = credential["password"].clone();
        }
        for field in ["randCode", "smsCode", "otpCode"] {
            request[field] = challenge[field].clone();
        }
        tracing::info!(target:"app_replay_auth",user_id=%user.id,"authenticating with shared IAAA binding");
        self.request(request).await
    }

    fn with_user_agent(user_agent: String) -> Result<Self, String> {
        let auth = AppAuth::new(&user_agent)?;
        let api_client = Client::builder()
            .user_agent(&user_agent)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(25))
            .build()
            .map_err(|_| "创建 App API 客户端失败")?;
        tracing::info!(user_agent=%user_agent,"好学 App 客户端使用 User-Agent（完整值）");
        Ok(Self {
            auth,
            api_client,
            user_agent,
            user: None,
            user_id: None,
            invalid_session: std::sync::atomic::AtomicBool::new(false),
            bound_user: None,
        })
    }

    pub async fn request(&mut self, request: Value) -> Result<Value, String> {
        let account = request["account"].as_str().unwrap_or("");
        let result = match request["action"].as_str().unwrap_or("") {
            "prepare" => self.auth.prepare(account).await,
            "captcha" => self.auth.captcha().await,
            "sms" => self.auth.sms(account).await,
            "login" => match self.auth.login(&request).await {
                Ok(user) => {
                    self.user = Some(user);
                    self.invalid_session
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    self.persist_session().map(|_| json!({"logged_in":true}))
                }
                Err(error) => Err(error),
            },
            "status" => Ok(json!({"logged_in":self.user.is_some()})),
            "scan" => self.scan(&request).await,
            _ => Err("未知 App 操作".into()),
        };
        if self
            .invalid_session
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            self.user = None;
            if let Some(id) = self.user_id {
                super::app_replay_vault::remove(id, "session")?;
            }
        }
        if let Err(error) = &result {
            tracing::error!(target:"app_replay_auth",action=request["action"].as_str().unwrap_or("unknown"),error=%error,"App request failed");
        }
        // Persist encrypted credentials server-side; never expose UserInfo to UI.
        Ok(match result {
            Ok(data) => json!({"ok":true,"data":data}),
            Err(error) => json!({"ok":false,"error":error,"diagnostics":self.auth.diagnostics}),
        })
    }

    async fn api(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, String> {
        tracing::info!(target:"app_replay_download",%path,"requesting signed course API");
        let token = self
            .user
            .as_ref()
            .and_then(|v| v["_token"].as_str())
            .filter(|s| !s.is_empty())
            .ok_or("请先登录好学 App")?;
        let mut url = Url::parse(&format!("https://yjapise.pku.edu.cn/courseapi/{path}")).unwrap();
        url.query_pairs_mut().extend_pairs(signed(query).iter());
        let mut response = self
            .api_client
            .get(url)
            .header("Accept", "application/json")
            .header("token", token)
            .send()
            .await
            .map_err(|_| "无法连接学校课程服务器，请检查校园网或 VPN")?;
        if !response.status().is_success() {
            if matches!(response.status().as_u16(),401 | 403) {
                self.invalid_session
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            return Err(format!(
                "课程服务器返回 HTTP {}",
                response.status().as_u16()
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "读取课程响应失败")? {
            if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
                return Err("课程响应超过大小限制".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let data: Value =
            serde_json::from_slice(&bytes).map_err(|_| "课程服务器返回了无法识别的数据格式")?;
        if !data.is_object() {
            return Err("课程服务器返回了无法识别的数据格式".into());
        }
        let code = data["code"].as_i64().or_else(||data["code"].as_str().and_then(|code|code.parse().ok()));
        if matches!(code,Some(401 | 403)) {
            self.invalid_session.store(true,std::sync::atomic::Ordering::Relaxed);
            return Err("好学 App 登录已过期".into());
        }
        if data["success"] == false {
            return Err(data["message"]
                .as_str()
                .or(data["msg"].as_str())
                .unwrap_or("请求被服务器拒绝")
                .into());
        }
        Ok(data)
    }

    async fn video_url(&self, course: &str, sub: &str) -> Result<String, String> {
        let response = self
            .api(
                "v3/course/get-course-sub-detail",
                &[("course_id", course), ("sub_id", sub)],
            )
            .await?;
        let url = playback_url(&response["data"])?;
        super::app_replay_media::validated_addresses(
            &Url::parse(&url).map_err(|_| "视频地址无效")?,
        )
        .await?;
        Ok(url)
    }

    async fn scan_date(&self, date: &str, name: &str, mapping: &str) -> Result<Vec<Value>, String> {
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        let mut finished = false;
        for page in 1..=100 {
            let page_string = page.to_string();
            let response = self
                .api(
                    "v3/course/get-use-member-sub",
                    &[("course_time", date), ("page", &page_string)],
                )
                .await?;
            let data = &response["data"];
            let batch = data["lists"]
                .as_array()
                .ok_or("课次列表格式与 APK 模型不一致")?;
            let fresh: Vec<_> = batch
                .iter()
                .filter(|r| {
                    !seen.contains(&(string_value(&r["course_id"]), string_value(&r["sub_id"])))
                })
                .collect();
            if !batch.is_empty() && fresh.is_empty() {
                return Err("回放列表出现重复分页，查询已停止".into());
            }
            for row in fresh {
                seen.insert((
                    string_value(&row["course_id"]),
                    string_value(&row["sub_id"]),
                ));
                let matches = if mapping.is_empty() {
                    normalized(row["course_name"].as_str().unwrap_or("")) == normalized(name)
                } else {
                    string_value(&row["course_id"]) == mapping
                };
                if matches {
                    rows.push(row.clone());
                }
            }
            let size = number(&data["pageSize"]).filter(|n| *n > 0).unwrap_or(20);
            if batch.is_empty() || page * size >= number(&data["count"]).unwrap_or(0) {
                finished = true;
                break;
            }
        }
        if !finished {
            return Err("回放列表超过最大分页数".into());
        }
        if rows
            .iter()
            .map(|r| string_value(&r["course_id"]))
            .collect::<HashSet<_>>()
            .len()
            > 1
        {
            return Err("好学 App 有多门同名课程，请在学期配置中设置 app_course_ids 映射".into());
        }
        let mut result = Vec::new();
        for row in rows {
            let course = string_value(&row["course_id"]);
            let sub = string_value(&row["sub_id"]);
            if course.is_empty() || sub.is_empty() {
                return Err("课次缺少 course_id/sub_id".into());
            }
            let (url, error) = match self.video_url(&course, &sub).await {
                Ok(url) => (url, Value::Null),
                Err(e) => (String::new(), json!(e)),
            };
            result.push(json!({"course_id":course,"sub_id":sub,"video_url":url,"time":row.get("start_at").filter(|v|!v.is_null()).or(row.get("sub_name")).map(string_value).unwrap_or_default(),"error":error}));
        }
        Ok(result)
    }

    async fn scan(&self, request: &Value) -> Result<Value, String> {
        if self.user.is_none() {
            return Err("请先登录好学 App".into());
        }
        let dates = request["dates"].as_array().ok_or("缺少查询日期")?;
        let name = request["course_name"].as_str().unwrap_or("");
        let mapping = request["app_course_id"].as_str().unwrap_or("");
        let mut result = Vec::new();
        for date in dates {
            let date = date.as_str().ok_or("查询日期格式无效")?;
            match self.scan_date(date, name, mapping).await {
                Ok(rows) => {
                    let error = if rows.is_empty() {
                        json!("该日期没有此课程的可访问回放")
                    } else {
                        Value::Null
                    };
                    result.push(json!({"date":date,"lessons":rows,"error":error}));
                }
                Err(error) => {
                    tracing::warn!(target:"app_replay_download",%date,%error,"date replay query failed");
                    result.push(json!({"date":date,"lessons":[],"error":error}));
                }
            }
        }
        Ok(json!(result))
    }

    pub async fn download(&mut self, replay: &ManualReplay, output: &Path) -> Result<(), String> {
        self.download_with_progress(replay, output, None).await
    }

    pub async fn download_with_progress(
        &mut self,
        replay: &ManualReplay,
        output: &Path,
        progress: Option<std::sync::Arc<std::sync::atomic::AtomicU8>>,
    ) -> Result<(), String> {
        let media = self.prepare_media(replay).await?;
        super::pkuvideo::download_prepared(&media, output, 3, 120, progress).await
    }

    pub async fn prepare_media(&mut self, replay: &ManualReplay) -> Result<super::prepared_media::PreparedMedia, String> {
        tracing::info!(target:"app_replay_download",course_id=%replay.course_id,sub_id=%replay.sub_id,date=%replay.date,"refreshing date replay list before playback");
        let date = if replay.date.is_empty() {
            return Err("该手动课节缺少日期，请刷新课节列表后重试".into());
        } else {
            &replay.date
        };
        let rows = self.scan_date(date, "", &replay.course_id).await;
        let rows = if self
            .invalid_session
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            self.user = None;
            if let Some(id) = self.user_id {
                super::app_replay_vault::remove(id, "session")?;
            }
            tracing::warn!(target:"app_replay_auth","persistent App session expired; reauthenticating once");
            let user = self
                .bound_user
                .clone()
                .ok_or("App 登录已过期，请重新登录")?;
            let login = self.login_bound(&user, &json!({})).await?;
            if login["ok"] != true {
                return Err(login["error"]
                    .as_str()
                    .unwrap_or("App 登录已过期，需要验证码")
                    .into());
            }
            self.scan_date(date, "", &replay.course_id).await?
        } else {
            rows?
        };
        let row = rows
            .iter()
            .find(|r| r["sub_id"].as_str() == Some(replay.sub_id.as_str()))
            .ok_or("对应日期已无此课次的可访问回放")?;
        if let Some(error) = row["error"].as_str() {
            return Err(error.into());
        }
        // scan_date has refreshed the signed detail for this date/sub_id. Do not
        // reuse the URL saved when adding, or fetch another one-time URL afterwards.
        let url = row["video_url"]
            .as_str()
            .ok_or("对应日期回放缺少地址")?
            .to_string();
        self.media_from_url(url).await
    }

    pub async fn prepare_course_lesson(&mut self,user:&crate::store::FileUser,course_name:&str,
        mapping:&str,date:&str,time:&str)->Result<super::prepared_media::PreparedMedia,String> {
        date.parse::<chrono::NaiveDate>().map_err(|_|"课节日期无效，无法匹配好学 App 回放")?;
        tracing::info!(target:"app_replay_auth",user_id=%user.id,%date,"回退前检查好学 App 登录及课程 API 会话状态");
        let status=self.login_bound(user,&json!({})).await?;
        if status["data"]["logged_in"]!=true {
            return Err(format!("请先完成好学 App 登录验证后重试：{}",status["error"].as_str().unwrap_or("App 未登录")));
        }
        for attempt in 0..2 {
            let result=async {
                let rows=self.scan_date(date,course_name,mapping).await?;
                let row=matching_course_replay(&rows,time)?;
                if let Some(error)=row["error"].as_str() {return Err(error.to_string());}
                let url=row["video_url"].as_str().filter(|url|!url.is_empty()).ok_or("好学 App 未返回有效回放链接")?.to_string();
                self.media_from_url(url).await
            }.await;
            let expired=self.invalid_session.swap(false,std::sync::atomic::Ordering::Relaxed);
            if expired && attempt==0 {
                self.user=None;
                if let Some(id)=self.user_id {super::app_replay_vault::remove(id,"session")?;}
                tracing::warn!(target:"app_replay_auth","回退获取发现 App 会话过期，重新认证一次");
                let status=self.login_bound(user,&json!({})).await?;
                if status["data"]["logged_in"]!=true {
                    return Err(format!("好学 App 会话已过期，请完成登录验证：{}",status["error"].as_str().unwrap_or("App 未登录")));
                }
            } else {
                if result.is_ok() {tracing::info!(target:"app_replay_auth",user_id=%user.id,"好学 App 回放及会话检查通过");}
                if expired {self.invalid_session.store(true,std::sync::atomic::Ordering::Relaxed);}
                return result;
            }
        }
        unreachable!("bounded App authentication refresh")
    }

    async fn media_from_url(&self,url:String)->Result<super::prepared_media::PreparedMedia,String> {
        let token = self
            .user
            .as_ref()
            .and_then(|v| v["_cookieToken"].as_str())
            .unwrap_or("")
            .to_string();
        let (url, enc_key) = if url.contains(".m3u8") {
            let (url, playlist) = super::prepared_media::public_playlist(&url, 60).await?;
            let uri = super::pkuvideo::key_uri(&url, &playlist)?;
            let key = super::app_replay_media::fetch_key(&uri, &token, &self.user_agent).await.map_err(|error|{
                if error.contains("HTTP 401") || error.contains("HTTP 403") {
                    self.invalid_session.store(true,std::sync::atomic::Ordering::Relaxed);
                }
                error
            })?;
            (url, Some(key))
        } else { (url, None) };
        Ok(super::prepared_media::PreparedMedia { url, enc_key })
    }
}

fn replay_start_minutes(time:&str)->Option<i32> {
    if let Ok(stamp)=time.parse::<i64>() {
        let dt=if time.len()>=13 {chrono::DateTime::from_timestamp_millis(stamp)} else {chrono::DateTime::from_timestamp(stamp,0)};
        if let Some(dt)=dt {
            use chrono::Timelike;
            let local=dt.with_timezone(&chrono::FixedOffset::east_opt(8*3600).unwrap());
            return Some((local.hour()*60+local.minute()) as i32);
        }
    }
    let minutes=super::lesson_parser::parse_time_order(time);
    (minutes>0 && (time.contains('第') || time.contains(':'))).then_some(minutes)
}
fn matching_course_replay<'a>(rows:&'a [Value],time:&str)->Result<&'a Value,String> {
    if rows.is_empty() {return Err("好学 App 中该课程在此日期没有可访问回放".into());}
    if rows.len()==1 {return Ok(&rows[0]);}
    let target=replay_start_minutes(time).ok_or("同一天有多节回放，但教学网课节缺少可识别的上课时间")?;
    let matches:Vec<_>=rows.iter().filter(|row|row["time"].as_str().and_then(replay_start_minutes)==Some(target)).collect();
    match matches.as_slice() {
        [row]=>Ok(row),
        []=>Err("好学 App 回放无法匹配该课节的上课时间".into()),
        _=>Err("好学 App 有多条同时间回放，无法唯一匹配此课节".into()),
    }
}

pub(super) fn signed(query: &[(&str, &str)]) -> BTreeMap<String, String> {
    let mut params: BTreeMap<String, String> = query
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    for (k, v) in [
        ("tenantid", "d438cafb4c34b3b49b3a523bbef50203"),
        ("tenantId", "d438cafb4c34b3b49b3a523bbef50203"),
        ("api_version", "3.12.0"),
        ("app_id", "50"),
        ("client", "android"),
    ] {
        params.insert(k.into(), v.into());
    }
    let mut input = String::new();
    for (k, v) in &params {
        input.push_str(k);
        input.push_str(v);
    }
    input.push_str("8225ca5fa4d56d6fde540db6024fba39");
    params.insert(
        "sign".into(),
        format!("{:x}", Md5::digest(input.as_bytes())),
    );
    params
}
fn normalized(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}
fn number(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}
fn string_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        _ => v.to_string(),
    }
}

fn playback_url(data: &Value) -> Result<String, String> {
    let mut url = None;
    if let Some(variants) = data["videoArr"].as_array() {
        url = variants.iter().find_map(|v| {
            v["path"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });
    }
    if url.is_none() {
        let content = if let Some(s) = data["sub_content"].as_str() {
            serde_json::from_str::<Value>(s).map_err(|_| "回放内容格式无效")?
        } else {
            data["sub_content"].clone()
        };
        url = content["save_playback"]["contents"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }
    let url = url.ok_or("此课次暂无回放，或没有播放权限")?;
    super::app_replay_media::check_media_url(&Url::parse(&url).map_err(|_| "不支持此视频地址")?)?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn client_is_native_and_does_not_expose_session() {
        let mut client = AppReplayClient::with_user_agent("Mozilla/5.0 (Linux; Android 14) test-pool-agent".into()).unwrap();
        assert_eq!(
            client.request(json!({"action":"status"})).await.unwrap(),
            json!({"ok":true,"data":{"logged_in":false}})
        );
        client.user = Some(json!({"_token":"secret-api","_cookieToken":"secret-media"}));
        let status = client.request(json!({"action":"status"})).await.unwrap();
        assert!(!status.to_string().contains("secret"));
        assert!(AppReplayClient::with_user_agent("bad\r\nagent".into()).is_err());
    }
    #[test]
    fn fallback_matches_course_date_rows_by_period_or_clock_without_guessing() {
        let rows=vec![json!({"sub_id":"morning","time":"2026-10-13 08:00:00"}),json!({"sub_id":"later","time":"2026-10-13 10:10:00"})];
        assert_eq!(matching_course_replay(&rows,"第3-4节").unwrap()["sub_id"],"later");
        assert_eq!(matching_course_replay(&rows,"08:00").unwrap()["sub_id"],"morning");
        assert!(matching_course_replay(&rows,"第7-9节").is_err());
        assert!(matching_course_replay(&[],"08:00").is_err());
        let duplicate=vec![rows[0].clone(),rows[0].clone()];
        assert!(matching_course_replay(&duplicate,"第1-2节").is_err());
    }

    #[test]
    fn signed_and_video_fallback_match_zip() {
        let params = signed(&[("course_time", "2026-10-13"), ("page", "1")]);
        assert_eq!(params["sign"], "958735168d4bf1f7fc85336c86867a23");
        assert_eq!(playback_url(&json!({"videoArr":[{"path":"https://media.example/a.mp4"}],"sub_content":{"save_playback":{"contents":"https://media.example/b.m3u8"}}})).unwrap(),"https://media.example/a.mp4");
        assert_eq!(playback_url(&json!({"sub_content":"{\"save_playback\":{\"contents\":\"https://media.example/b.m3u8\"}}"})).unwrap(),"https://media.example/b.m3u8");
        assert!(playback_url(&json!({})).is_err());
        assert_eq!(normalized("线性 代数"), normalized("线性代数"));
    }
}
