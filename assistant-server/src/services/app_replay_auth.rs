//! HTTP IAAA/Passport authentication ported from the supplied ZIP, without JS execution.
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::{cookie::Jar, redirect::Policy, Client, Url};
use rsa::{pkcs1::DecodeRsaPublicKey, pkcs8::DecodePublicKey, Pkcs1v15Encrypt, RsaPublicKey};
use scraper::{Html, Selector};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

const PASSPORT: &str = "https://passport.pku.edu.cn";
const IAAA: &str = "https://iaaa.pku.edu.cn/iaaa/";
const LOGIN: &str =
    "https://passport.pku.edu.cn/auth/login?redirect=https://passport.pku.edu.cn/auth/app-login";

pub struct AppAuth {
    client: Client,
    last_url: String,
    ready: bool,
    require_captcha: bool,
    last_sms: Option<Instant>,
    pub diagnostics: Vec<Value>,
}

pub fn check_auth_url(url: &Url) -> Result<(), String> {
    if url.scheme() != "https"
        || !matches!(
            url.host_str(),
            Some("passport.pku.edu.cn" | "iaaa.pku.edu.cn" | "resource.pku.edu.cn")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
    {
        return Err("认证服务返回了非预期地址，已停止请求".into());
    }
    Ok(())
}

fn safe_route(url: &Url) -> String {
    if check_auth_url(url).is_err() {
        return "[外部地址]".into();
    }
    let path = url.path();
    let sensitive = path.len() > 160
        || regex::Regex::new(r"\d{6,}|[A-Za-z0-9_-]{40,}")
            .unwrap()
            .is_match(path);
    format!(
        "{}{}",
        url.host_str().unwrap_or_default(),
        if sensitive {
            "[已隐藏动态路径]"
        } else {
            path
        }
    )
}

impl AppAuth {
    pub fn new(user_agent: &str) -> Result<Self, String> {
        let client = Client::builder()
            .user_agent(user_agent)
            .cookie_provider(Arc::new(Jar::default()))
            .redirect(Policy::none())
            .timeout(Duration::from_secs(25))
            .build()
            .map_err(|_| "创建认证 HTTP 客户端失败")?;
        Ok(Self {
            client,
            last_url: String::new(),
            ready: false,
            require_captcha: false,
            last_sms: None,
            diagnostics: Vec::new(),
        })
    }

    async fn fetch(
        &mut self,
        target: &str,
        form: Option<&BTreeMap<String, String>>,
        binary: bool,
    ) -> Result<Vec<u8>, String> {
        let mut url = Url::parse(target).map_err(|_| "认证地址无效")?;
        let mut form = form.cloned();
        let referer = Url::parse(&self.last_url)
            .ok()
            .filter(|u| u.host_str() == Some("iaaa.pku.edu.cn"))
            .map(|u| u.to_string())
            .unwrap_or_else(|| format!("{IAAA}oauth.jsp"));
        for _ in 0..11 {
            check_auth_url(&url)?;
            tracing::info!(target:"app_replay_auth", route=%safe_route(&url), method=if form.is_some(){"POST"}else{"GET"}, "request");
            let request = if let Some(values) = &form {
                self.client.post(url.clone()).form(values).header(
                    "Content-Type",
                    "application/x-www-form-urlencoded; charset=UTF-8",
                )
            } else {
                self.client.get(url.clone())
            };
            let mut response = request
                .header("Referer", &referer)
                .send()
                .await
                .map_err(|_| "连接认证服务器失败，请检查校园网或 VPN")?;
            if response.status().is_redirection() {
                let destination = response
                    .headers()
                    .get("location")
                    .and_then(|h| h.to_str().ok())
                    .ok_or("认证重定向缺少地址")?;
                let next = url.join(destination).map_err(|_| "认证重定向地址无效")?;
                check_auth_url(&next)?;
                let status = response.status().as_u16();
                tracing::info!(target:"app_replay_auth", status, from=%safe_route(&url), to=%safe_route(&next), "redirect");
                if form.is_some() && matches!(status, 307 | 308) {
                    return Err("认证服务器返回了不支持的 POST 重定向".into());
                }
                if matches!(status, 301 | 302 | 303) {
                    form = None;
                }
                url = next;
                continue;
            }
            let status = response.status().as_u16();
            if !response.status().is_success() {
                return Err(format!("认证服务器返回 HTTP {status}"));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| "读取认证响应失败")?
            {
                if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                    return Err("认证响应超过大小限制".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            self.last_url = url.to_string();
            if binary {
                tracing::info!(target:"app_replay_auth", route=%safe_route(&url), status, bytes=bytes.len(), "binary response");
            } else {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| "认证响应不是 UTF-8")?
                    .trim_start_matches('\u{feff}');
                let summary = response_summary(&url, status, text);
                tracing::info!(target:"app_replay_auth", summary=%summary, "response");
                self.diagnostics.push(summary);
                if self.diagnostics.len() > 8 {
                    self.diagnostics.remove(0);
                }
            }
            return Ok(bytes);
        }
        Err("认证重定向次数过多".into())
    }

    async fn text(
        &mut self,
        url: &str,
        form: Option<&BTreeMap<String, String>>,
    ) -> Result<String, String> {
        String::from_utf8(self.fetch(url, form, false).await?)
            .map(|s| s.trim_start_matches('\u{feff}').to_string())
            .map_err(|_| "认证响应不是 UTF-8".into())
    }

    async fn endpoint(
        &mut self,
        path: &str,
        query: &[(&str, String)],
        form: Option<&BTreeMap<String, String>>,
    ) -> Result<Value, String> {
        let mut url = Url::parse(&format!("{IAAA}{path}")).unwrap();
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        }
        let data: Value = serde_json::from_str(&self.text(url.as_str(), form).await?)
            .map_err(|_| "IAAA 响应格式无法识别")?;
        if !data.is_object() {
            return Err("IAAA 响应格式无法识别".into());
        }
        Ok(data)
    }

    async fn initialize(&mut self) -> Result<(), String> {
        if self.ready {
            return Ok(());
        }
        let text = self.text(LOGIN, None).await?;
        let (action, form) = login_form(&text)?;
        self.text(&action, Some(&form)).await?;
        self.require_captcha = self.endpoint("isShowCode.do", &[], None).await?["success"] == true;
        self.ready = true;
        Ok(())
    }

    pub async fn prepare(&mut self, account: &str) -> Result<Value, String> {
        self.initialize().await?;
        let policy = if account.is_empty() {
            json!({})
        } else {
            self.endpoint(
                "isMobileAuthen.do",
                &[
                    ("userName", account.into()),
                    ("appId", "resource".into()),
                    ("_rand", random_fraction()),
                ],
                None,
            )
            .await?
        };
        let mode = if truthy(&policy["isMobileAuthen"]) {
            policy["authenMode"].as_str().unwrap_or("")
        } else {
            ""
        };
        if mode == "OTP" && policy["isBind"] == false {
            return Err("学校要求先绑定动态口令 App，请在官方页面完成绑定".into());
        }
        Ok(
            json!({"needCaptcha":self.require_captcha,"factor":mode,"email":truthy(&policy["emailSuitable"])}),
        )
    }

    pub async fn captcha(&mut self) -> Result<Value, String> {
        self.initialize().await?;
        let bytes = self
            .fetch(
                &format!("{IAAA}servlet/DrawServlet?Rand={}", epoch()),
                None,
                true,
            )
            .await?;
        let mime = if bytes.starts_with(b"\x89PNG") {
            "png"
        } else if bytes.starts_with(b"\xff\xd8") {
            "jpeg"
        } else if bytes.starts_with(b"GIF8") {
            "gif"
        } else {
            return Err("IAAA 验证码格式未识别".into());
        };
        Ok(json!({"image":format!("data:image/{mime};base64,{}",STANDARD.encode(bytes))}))
    }

    pub async fn sms(&mut self, account: &str) -> Result<Value, String> {
        self.initialize().await?;
        if self
            .last_sms
            .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
        {
            return Err("请等待 60 秒后再获取验证码".into());
        }
        if self.prepare(account).await?["factor"] != "SMS" {
            return Err("此账号当前不使用短信或邮件验证".into());
        }
        let response = self
            .endpoint(
                "sendSMSCode.do",
                &[
                    ("userName", account.into()),
                    ("appId", "resource".into()),
                    ("_rand", epoch()),
                ],
                None,
            )
            .await?;
        if response["success"] != true {
            return Err(rejected_message(&response));
        }
        self.last_sms = Some(Instant::now());
        Ok(
            json!({"message":format!("验证码已发至{}",response["mobileMask"].as_str().unwrap_or("已绑定的手机或邮箱"))}),
        )
    }

    pub async fn login(&mut self, request: &Value) -> Result<Value, String> {
        self.initialize().await?;
        let account = request["account"].as_str().unwrap_or("");
        let password = request["password"].as_str().unwrap_or("");
        if account.is_empty()
            || (password.is_empty()
                && request["encrypted_password"]
                    .as_str()
                    .unwrap_or("")
                    .is_empty())
        {
            return Err("请填写统一认证账号和密码".into());
        }
        let policy = self.prepare(account).await?;
        for (required, field, message) in [
            (
                policy["needCaptcha"] == true,
                "randCode",
                "请输入图形验证码",
            ),
            (
                policy["factor"] == "SMS",
                "smsCode",
                "请输入短信或邮件验证码",
            ),
            (policy["factor"] == "OTP", "otpCode", "请输入动态口令"),
        ] {
            if required && request[field].as_str().unwrap_or("").is_empty() {
                return Err(message.into());
            }
        }
        // Fetch the live key each time; legacy ciphertext is usable only for that exact key.
        let key = self.endpoint("getPublicKey.do", &[], None).await?;
        if key["success"] != true {
            return Err("无法获取 IAAA RSA 公钥".into());
        }
        let public_key = key["key"].as_str().ok_or("IAAA 未返回公钥")?;
        let encrypted = login_password(public_key, request)?;
        let mut form: BTreeMap<String, String> = [
            ("appid", "resource"),
            ("userName", account),
            ("password", encrypted.as_str()),
            ("randCode", request["randCode"].as_str().unwrap_or("")),
            ("smsCode", request["smsCode"].as_str().unwrap_or("")),
            ("otpCode", request["otpCode"].as_str().unwrap_or("")),
            ("remTrustChk", "false"),
            ("redirUrl", PASSPORT),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        let result = self.endpoint("oauthlogin.do", &[], Some(&form)).await;
        form.clear();
        let result = result?;
        if result["success"] != true {
            self.require_captcha |=
                truthy(&result["showCode"]) || result["errors"]["code"] == "E03";
            return Err(rejected_message(&result));
        }
        tracing::info!(target:"app_replay_auth","stage iaaa_accepted");
        let ticket = result["token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("IAAA 未返回有效票据")?;
        self.diagnostics.clear();
        let mut callback = Url::parse(PASSPORT).unwrap();
        callback
            .query_pairs_mut()
            .append_pair("_rand", &random_fraction())
            .append_pair("token", ticket);
        let mut text = self.text(callback.as_str(), None).await?;
        let mut user = extract_user(&text);
        let mut seen = HashSet::new();
        for _ in 0..4 {
            if user.is_some() {
                break;
            }
            let Some(next) = page_redirect(&text, &self.last_url) else {
                break;
            };
            if !seen.insert(next.clone()) {
                break;
            }
            text = self.text(&next, None).await?;
            user = extract_user(&text);
        }
        if user.is_none() {
            user = extract_user(
                &self
                    .text(&format!("{PASSPORT}/auth/app-login"), None)
                    .await?,
            );
        }
        let Some(user) = user else {
            tracing::warn!(target:"app_replay_auth","stage app_session_missing; callback_parse_failed");
            return Err(
                "IAAA 已通过，但尚未取得 App 会话。请复制下方“认证诊断”信息以定位回调。".into(),
            );
        };
        for field in ["_token", "_cookieToken"] {
            if user[field]
                .as_str()
                .is_some_and(|s| s.contains(['\r', '\n']))
            {
                return Err("无效的会话格式".into());
            }
        }
        tracing::info!(target:"app_replay_auth","stage app_session_received");
        Ok(user)
    }
}

fn random_fraction() -> String {
    format!("{:.16}", rand::random::<f64>())
}
fn epoch() -> String {
    (chrono::Utc::now().timestamp_millis() as f64 / 1000.0).to_string()
}
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
fn rejected_message(v: &Value) -> String {
    v["errors"]["msg"]
        .as_str()
        .or(v["message"].as_str())
        .unwrap_or("统一认证失败")
        .into()
}
fn login_password(public_key: &str, request: &Value) -> Result<String, String> {
    let password = request["password"].as_str().unwrap_or("");
    if !password.is_empty() {
        return encrypt_password(public_key, password);
    }
    if request["stored_public_key"].as_str().map(str::trim) != Some(public_key.trim()) {
        return Err("绑定只保存了旧公钥密文，公钥已变更；请在 IAAA 绑定页重新绑定一次，之后好学 App 将自动登录".into());
    }
    request["encrypted_password"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or("请先绑定 IAAA 账号".into())
}
pub(super) fn encrypt_password(pem: &str, password: &str) -> Result<String, String> {
    let key = RsaPublicKey::from_public_key_pem(pem)
        .or_else(|_| RsaPublicKey::from_pkcs1_pem(pem))
        .map_err(|_| "IAAA RSA 公钥格式无效")?;
    let bytes = key
        .encrypt(&mut rand::rngs::OsRng, Pkcs1v15Encrypt, password.as_bytes())
        .map_err(|_| "RSA 密码加密失败")?;
    Ok(STANDARD.encode(bytes))
}

fn login_form(text: &str) -> Result<(String, BTreeMap<String, String>), String> {
    let document = Html::parse_document(text);
    let selector = Selector::parse("form#form-pku-login").unwrap();
    let form = document
        .select(&selector)
        .next()
        .ok_or("未找到官方认证隐藏表单")?;
    let action = form.value().attr("action").unwrap_or("").to_string();
    let values: BTreeMap<String, String> = form
        .select(&Selector::parse("input[name]").unwrap())
        .map(|input| {
            (
                input.value().attr("name").unwrap().into(),
                input.value().attr("value").unwrap_or("").into(),
            )
        })
        .collect();
    if values.get("appID").map(String::as_str) != Some("resource")
        || values.get("redirectUrl").map(String::as_str) != Some(PASSPORT)
    {
        return Err("官方应用 ID 或回调地址已变化，需要更新认证配置".into());
    }
    let url = Url::parse(&action).map_err(|_| "官方认证入口已变化")?;
    check_auth_url(&url)?;
    if action != format!("{IAAA}oauth.jsp") {
        return Err("官方认证入口已变化".into());
    }
    Ok((action, values))
}

fn response_summary(url: &Url, status: u16, text: &str) -> Value {
    let doc = Html::parse_document(text);
    let route = |s: &str| {
        url.join(s)
            .map(|u| safe_route(&u))
            .unwrap_or_else(|_| "[无效地址]".into())
    };
    let scripts: Vec<_> = doc
        .select(&Selector::parse("script[src]").unwrap())
        .take(12)
        .map(|e| route(e.value().attr("src").unwrap()))
        .collect();
    let forms:Vec<_>=doc.select(&Selector::parse("form").unwrap()).take(4).map(|e|json!({"route":route(e.value().attr("action").unwrap_or("")),"method":e.value().attr("method").unwrap_or("get").to_uppercase()})).collect();
    json!({"route":safe_route(url),"status":status,"length":text.len(),"markers":{"app_token":text.contains("_token"),"media_token":text.contains("_cookieToken"),"bridge_callback":text.contains("PKULoginSuccess"),"iaaa_login_form":text.contains("form-pku-login")},"scripts":scripts,"forms":forms,"meta_refresh":doc.select(&Selector::parse("meta[http-equiv]").unwrap()).any(|e|e.value().attr("http-equiv").unwrap_or("").eq_ignore_ascii_case("refresh")),"location_redirect":regex::Regex::new(r"(?:window\.)?location(?:\.href|\.replace|\.assign)?\s*(?:=|\()").unwrap().is_match(text)})
}

// Only JSON, quoted JS strings, JSON.parse/stringify wrappers and variables
// assigned literal values are read. No JavaScript is evaluated.
fn js_string(source: &str) -> Option<(String, usize)> {
    let quote = source.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let mut chars = source.char_indices();
    chars.next();
    let mut out = String::new();
    while let Some((i, ch)) = chars.next() {
        if ch == quote {
            return Some((out, i + ch.len_utf8()));
        }
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let (_, escaped) = chars.next()?;
        match escaped {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            '\n' => {}
            '\r' => {
                if chars.clone().next().is_some_and(|(_, c)| c == '\n') {
                    chars.next();
                }
            }
            'u' | 'x' => {
                let count = if escaped == 'u' { 4 } else { 2 };
                let mut hex = String::new();
                for _ in 0..count {
                    hex.push(chars.next()?.1);
                }
                let mut code = u32::from_str_radix(&hex, 16).ok()?;
                if (0xd800..=0xdbff).contains(&code) {
                    if chars.next()?.1 != '\\' || chars.next()?.1 != 'u' {
                        return None;
                    }
                    let mut low = String::new();
                    for _ in 0..4 {
                        low.push(chars.next()?.1);
                    }
                    let low = u32::from_str_radix(&low, 16).ok()?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return None;
                    }
                    code = 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
                }
                out.push(char::from_u32(code)?);
            }
            other => out.push(other),
        }
    }
    None
}

fn literal(source: &str) -> Option<Value> {
    let source = source.trim_start();
    if let Some((s, _)) = js_string(source) {
        return serde_json::from_str(&s).ok().or(Some(Value::String(s)));
    }
    serde_json::Deserializer::from_str(source)
        .into_iter::<Value>()
        .next()?
        .ok()
}

fn resolve_value(source: &str, text: &str, depth: usize) -> Option<Value> {
    if depth > 5 {
        return None;
    }
    let source = source.trim_start();
    let wrapped = regex::Regex::new(r"^JSON\.(?:parse|stringify)\s*\(\s*").unwrap();
    if let Some(m) = wrapped.find(source) {
        return resolve_value(&source[m.end()..], text, depth + 1);
    }
    if let Some(v) = literal(source) {
        return Some(v);
    }
    let names = regex::Regex::new(r"^([A-Za-z_$][\w$]*)\s*(?:[);,]|$)").unwrap();
    let name = names.captures(source)?.get(1)?.as_str();
    let pattern = format!(
        r"(?:^|[^\w$.])(?:var\s+|let\s+|const\s+)?{}\s*=\s*",
        regex::escape(name)
    );
    let re = regex::Regex::new(&pattern).ok()?;
    let m = re
        .find_iter(text)
        .filter(|m| !text[m.end()..].starts_with('='))
        .last()?;
    resolve_value(&text[m.end()..], text, depth + 1)
}

fn find_user(value: &Value, depth: usize) -> Option<Value> {
    if depth > 4 {
        return None;
    }
    if let Some(text) = value.as_str() {
        return find_user(&serde_json::from_str::<Value>(text).ok()?, depth + 1);
    }
    if value["_token"].as_str().is_some_and(|s| !s.is_empty()) {
        return Some(value.clone());
    }
    for key in ["data", "user", "userInfo", "userinfo", "result"] {
        if let Some(v) = value.get(key).and_then(|v| find_user(v, depth + 1)) {
            return Some(v);
        }
    }
    None
}

pub fn extract_user(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if let Some(user) = find_user(&v, 0) {
            return Some(user);
        }
    }
    let decoded = html_escape::decode_html_entities(text);
    let bridge=regex::Regex::new(r#"\b(?:window\.)?bridge\s*(?:\.\s*PKULoginSuccess|\[\s*['"]PKULoginSuccess['"]\s*\])\s*\(\s*"#).unwrap();
    let objects = regex::Regex::new(r#"\{\s*""#).unwrap();
    for candidate in [text, decoded.as_ref()] {
        for m in bridge.find_iter(candidate) {
            if let Some(user) =
                resolve_value(&candidate[m.end()..], candidate, 0).and_then(|v| find_user(&v, 0))
            {
                return Some(user);
            }
        }
        for m in objects.find_iter(candidate) {
            if let Some(user) = literal(&candidate[m.start()..]).and_then(|v| find_user(&v, 0)) {
                return Some(user);
            }
        }
    }
    None
}

fn page_redirect(text: &str, base: &str) -> Option<String> {
    let base = Url::parse(base).ok()?;
    let doc = Html::parse_document(text);
    let mut destinations = Vec::new();
    for e in doc.select(&Selector::parse("meta[http-equiv]").unwrap()) {
        if e.value()
            .attr("http-equiv")
            .unwrap_or("")
            .eq_ignore_ascii_case("refresh")
        {
            if let Some(content) = e.value().attr("content") {
                if let Some(m) = regex::Regex::new(r"(?i)url\s*=\s*(.+)")
                    .unwrap()
                    .captures(content)
                {
                    destinations.push(m[1].trim().trim_matches(['\'', '"']).to_string());
                }
            }
        }
    }
    let re=regex::Regex::new(r"(?:window\.)?location(?:\.href)?\s*=\s*|(?:window\.)?location\.(?:replace|assign)\s*\(\s*").unwrap();
    for m in re.find_iter(text) {
        if let Some((value, _)) = js_string(&text[m.end()..]) {
            destinations.push(value);
        }
    }
    destinations
        .into_iter()
        .filter_map(|s| base.join(&s).ok())
        .find(|u| {
            u.host_str() == Some("passport.pku.edu.cn")
                && u.path() != "/auth/login"
                && check_auth_url(u).is_ok()
        })
        .map(|u| u.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_ciphertext_requires_exact_live_public_key() {
        let request = serde_json::json!({"stored_public_key":" current-key ","encrypted_password":"legacy-cipher"});
        assert_eq!(
            super::login_password("current-key", &request).unwrap(),
            "legacy-cipher"
        );
        assert!(super::login_password("rotated-key", &request)
            .unwrap_err()
            .contains("重新绑定"));
        assert!(super::login_password(
            "current-key",
            &serde_json::json!({"stored_public_key":"current-key"})
        )
        .is_err());
    }
    use super::*;
    #[test]
    fn callback_literals_wrappers_variables_and_entities() {
        let user = json!({"_token":"api-test","_cookieToken":"media-test"});
        let raw = user.to_string();
        for text in [
            raw.clone(),
            format!("bridge.PKULoginSuccess({raw});"),
            format!("var info={raw}; bridge.PKULoginSuccess(JSON.stringify(info));"),
            format!(
                "window.bridge['PKULoginSuccess'](JSON.parse({}));",
                serde_json::to_string(&raw).unwrap()
            ),
            format!("bridge.PKULoginSuccess('{}');", raw),
            format!("bridge.PKULoginSuccess({});", raw.replace('"', "&quot;")),
        ] {
            assert_eq!(extract_user(&text), Some(user.clone()), "{text}");
        }
        assert!(extract_user("bridge.PKULoginSuccess(fetch('/secret'))").is_none());
    }
    #[test]
    fn rsa_uses_fresh_supplied_key() {
        use rsa::{pkcs8::EncodePublicKey, RsaPrivateKey};
        for _ in 0..2 {
            let private = RsaPrivateKey::new(&mut rand::rngs::OsRng, 1024).unwrap();
            let pem = private
                .to_public_key()
                .to_public_key_pem(Default::default())
                .unwrap();
            let encrypted = encrypt_password(&pem, "密码123").unwrap();
            assert_eq!(
                private
                    .decrypt(Pkcs1v15Encrypt, &STANDARD.decode(encrypted).unwrap())
                    .unwrap(),
                "密码123".as_bytes()
            );
        }
        assert!(encrypt_password("bad-key", "password").is_err());
    }
    #[test]
    fn auth_domains_forms_and_diagnostics_are_strict() {
        assert!(
            check_auth_url(&Url::parse("https://passport.pku.edu.cn@evil.example/").unwrap())
                .is_err()
        );
        assert!(check_auth_url(&Url::parse("http://iaaa.pku.edu.cn/").unwrap()).is_err());
        let form = r#"<form id="form-pku-login" action="https://iaaa.pku.edu.cn/iaaa/oauth.jsp"><input name="appID" value="resource"><input name="redirectUrl" value="https://passport.pku.edu.cn"></form>"#;
        assert_eq!(login_form(form).unwrap().1["appID"], "resource");
        assert!(login_form(&form.replace("resource", "blackboard")).is_err());
        let summary = response_summary(
            &Url::parse("https://passport.pku.edu.cn/auth/app-login?token=secret").unwrap(),
            200,
            "<script src='/a.js?token=secret'></script>",
        );
        assert!(!summary.to_string().contains("secret"));
        assert_eq!(
            page_redirect("window.location.href='/next';", PASSPORT),
            Some(format!("{PASSPORT}/next"))
        );
        assert!(page_redirect("window.location.href='https://evil.example/';", PASSPORT).is_none());
    }
}
