//! Private native Rust media proxy. Cookies never enter ffmpeg's command line.
use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use futures_util::StreamExt;
use reqwest::Url;
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

struct MediaState {
    download_id: uuid::Uuid,
    urls: Mutex<HashMap<String, Url>>,
    token: String,
    user_agent: String,
    progress: Option<Arc<std::sync::atomic::AtomicU8>>,
    segments: Mutex<HashMap<(String, Option<(u64, u64)>), bool>>,
    mp4_bytes: Mutex<(Vec<(u64, u64)>, u8)>,
}
impl MediaState {
    fn mp4_chunk(&self, start: u64, length: u64, total: u64) {
        if total == 0 || total == u64::MAX {
            return;
        }
        let mut bytes = self.mp4_bytes.lock().unwrap();
        bytes
            .0
            .push((start, start.saturating_add(length).min(total)));
        bytes.0.sort_unstable();
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for &(start, end) in &bytes.0 {
            if let Some(last) = ranges.last_mut().filter(|last| start <= last.1) {
                last.1 = last.1.max(end);
            } else {
                ranges.push((start, end));
            }
        }
        let downloaded: u64 = ranges
            .iter()
            .map(|(start, end)| end.saturating_sub(*start))
            .sum();
        bytes.0 = ranges;
        let percent = ((downloaded as u128 * 100 / total as u128) as u8).min(99);
        if percent > bytes.1 {
            bytes.1 = percent;
            if let Some(progress) = &self.progress {
                progress.fetch_max(percent, std::sync::atomic::Ordering::SeqCst);
            }
            tracing::info!(target:"app_replay_download",download_id=%self.download_id,downloaded,total,percent,"好学 MP4 下载进度");
        }
    }
    fn register_segments(&self, base: &Url, text: &str) {
        let mut segments = self.segments.lock().unwrap();
        let previous_total = segments.len();
        let mut is_segment = false;
        let mut range = None;
        let mut offset = 0u64;
        for line in text.lines().map(str::trim) {
            if line.starts_with("#EXTINF:") {
                is_segment = true;
            }
            if let Some(value) = line.strip_prefix("#EXT-X-BYTERANGE:") {
                let (length, start) = value
                    .split_once('@')
                    .map(|(l, s)| (l, s.parse().ok()))
                    .unwrap_or((value, None));
                if let Ok(length) = length.parse::<u64>() {
                    let start = start.unwrap_or(offset);
                    if let Some(end) = start.checked_add(length).and_then(|n| n.checked_sub(1)) {
                        range = Some((start, end));
                        offset = end + 1;
                    }
                }
            }
            if !line.is_empty() && !line.starts_with('#') && is_segment {
                if let Ok(url) = base.join(line) {
                    segments
                        .entry((url.to_string(), range.take()))
                        .or_insert(false);
                }
                is_segment = false;
            }
        }
        if segments.len() != previous_total {
            tracing::info!(target:"app_replay_download",download_id=%self.download_id,total=segments.len(),"好学 HLS 播放列表已读取，开始下载分片");
        }
    }
    fn complete_segment(&self, url: &str, served: Option<(u64, u64, u64)>) {
        let mut segments = self.segments.lock().unwrap();
        let mut newly_completed = 0;
        for ((target, range), done) in segments.iter_mut() {
            if target != url {
                continue;
            }
            let covered = match (range, served) {
                (Some((start, end)), Some((first, last, _))) => first <= *start && last >= *end,
                (None, Some((first, last, total))) => {
                    first == 0 && last.checked_add(1) == Some(total)
                }
                (_, None) => true,
            };
            if covered && !*done {
                *done = true;
                newly_completed += 1;
            }
        }
        if newly_completed > 0 {
            let completed = segments.values().filter(|done| **done).count();
            let total = segments.len();
            let percent = (completed * 100 / total).min(99);
            tracing::info!(target:"app_replay_download",download_id=%self.download_id,completed,total,percent,"好学 HLS 分片下载完成");
            if completed == total {
                tracing::info!(target:"app_replay_download",download_id=%self.download_id,percent=99,"好学 HLS 分片全部下载完成，正在合并视频");
            }
        }
        if let Some(progress) = &self.progress {
            if !segments.is_empty() {
                progress.fetch_max(
                    (segments.values().filter(|done| **done).count() * 100 / segments.len()) as u8,
                    std::sync::atomic::Ordering::SeqCst,
                );
            }
        }
    }
    fn register(&self, url: Url) -> Result<String, String> {
        check_media_url(&url)?;
        let mut urls = self.urls.lock().map_err(|_| "媒体注册锁异常")?;
        if let Some((id, _)) = urls.iter().find(|(_, old)| *old == &url) {
            return Ok(format!("/media/{id}"));
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        urls.insert(id.clone(), url);
        Ok(format!("/media/{id}"))
    }
}

pub(super) fn check_media_url(url: &Url) -> Result<(), String> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("不支持此视频地址".into());
    }
    Ok(())
}
fn global_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, d] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || (a == 255 && b == 255 && c == 255 && d == 255))
}
fn global_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => global_v4(v),
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return global_v4(v4);
            }
            let s = v.segments();
            !v.is_unspecified()
                && !v.is_loopback()
                && !v.is_multicast()
                && (s[0] & 0xfe00) != 0xfc00
                && (s[0] & 0xffc0) != 0xfe80
                && !(s[0] == 0x2001 && (s[1] == 0x0db8 || s[1] < 0x0200))
        }
    }
}

pub(super) async fn validated_addresses(url: &Url) -> Result<Vec<SocketAddr>, String> {
    check_media_url(url)?;
    let host = url.host_str().unwrap();
    let port = url.port_or_known_default().ok_or("媒体端口无效")?;
    let addresses: Vec<SocketAddr> = if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>()
    {
        vec![SocketAddr::new(ip, port)]
    } else {
        tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| "解析媒体服务器失败")?
            .collect()
    };
    if addresses.is_empty() || addresses.iter().any(|a| !global_ip(a.ip())) {
        return Err("视频地址不能指向本地或保留网络".into());
    }
    Ok(addresses)
}

fn media_request(
    client: &reqwest::Client,
    state: &MediaState,
    url: Url,
    send_cookie: bool,
    range: Option<&reqwest::header::HeaderValue>,
) -> reqwest::RequestBuilder {
    let mut request = client.get(url).header("User-Agent", &state.user_agent)
        .header("Origin", "https://onlineroomse.pku.edu.cn")
        .header("Referer", "https://onlineroomse.pku.edu.cn/");
    if send_cookie && !state.token.is_empty() {
        request = request.header("Cookie", format!("_token={}", state.token));
    }
    if let Some(range) = range {
        request = request.header("Range", range.clone());
    }
    request
}

async fn upstream(
    state: &MediaState,
    initial: Url,
    range: Option<reqwest::header::HeaderValue>,
) -> Result<reqwest::Response, String> {
    let mut url = initial;
    let mut send_cookie = true;
    for _ in 0..11 {
        let addresses = validated_addresses(&url).await?;
        let host = url.host_str().unwrap();
        // Pin the addresses checked above, instead of resolving again after validation.
        let client = reqwest::Client::builder()
            .user_agent(&state.user_agent)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(30))
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(|_| "创建媒体客户端失败")?;
        let request = media_request(&client, state, url.clone(), send_cookie, range.as_ref());
        let response = request
            .send()
            .await
            .map_err(|_| "无法连接媒体服务器，请检查校园网或 VPN")?;
        if response.status().is_redirection() {
            let target = response
                .headers()
                .get("location")
                .and_then(|h| h.to_str().ok())
                .ok_or("媒体重定向缺少地址")?;
            let next = url.join(target).map_err(|_| "媒体重定向地址无效")?;
            check_media_url(&next)?;
            if next.host_str() != url.host_str() {
                send_cookie = false;
            }
            url = next;
            continue;
        }
        if !response.status().is_success() {
            return Err(format!(
                "媒体服务器返回 HTTP {}",
                response.status().as_u16()
            ));
        }
        return Ok(response);
    }
    Err("媒体重定向次数过多".into())
}

fn rewrite_playlist(state: &MediaState, base: &Url, text: &str) -> Result<String, String> {
    state.register_segments(base, text);
    let uri = regex::Regex::new(r#"URI="([^"]+)""#).unwrap();
    let mut lines = Vec::new();
    for line in text.lines() {
        if !line.is_empty() && !line.starts_with('#') {
            lines.push(state.register(base.join(line.trim()).map_err(|_| "播放列表地址无效")?)?);
        } else if line.starts_with('#') {
            let mut rewritten = String::new();
            let mut position = 0;
            for captures in uri.captures_iter(line) {
                let m = captures.get(0).unwrap();
                rewritten.push_str(&line[position..m.start()]);
                let path = state.register(
                    base.join(&captures[1])
                        .map_err(|_| "播放列表引用地址无效")?,
                )?;
                rewritten.push_str(&format!("URI=\"{path}\""));
                position = m.end();
            }
            rewritten.push_str(&line[position..]);
            lines.push(rewritten);
        } else {
            lines.push(line.into());
        }
    }
    Ok(lines.join("\n") + "\n")
}

async fn media(
    State(state): State<Arc<MediaState>>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let url = state
        .urls
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .get(&id)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;
    let original_url = url.to_string();
    let mut response = upstream(&state, url, headers.get("range").cloned())
        .await
        .map_err(|error| {
            tracing::error!(target:"app_replay_download",%error,"media upstream failed");
            StatusCode::BAD_GATEWAY
        })?;
    let status = response.status();
    if !status.is_success() {
        tracing::error!(target:"app_replay_download",status=status.as_u16(),"media server rejected playback request");
    }
    let base = response.url().clone();
    let is_hls = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.to_lowercase().contains("mpegurl"))
        || base.path().to_lowercase().ends_with(".m3u8");
    if is_hls {
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?
        {
            if bytes.len() + chunk.len() > 4 * 1024 * 1024 {
                return Err(StatusCode::BAD_GATEWAY);
            }
            bytes.extend_from_slice(&chunk);
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| StatusCode::BAD_GATEWAY)?
            .trim_start_matches('\u{feff}');
        let body = rewrite_playlist(&state, &base, text).map_err(|_| StatusCode::BAD_GATEWAY)?;
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/vnd.apple.mpegurl")
            .header("content-length", body.len())
            .body(Body::from(body))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
    }
    let mut builder = Response::builder().status(status);
    for name in [
        "content-type",
        "content-length",
        "content-range",
        "accept-ranges",
    ] {
        if let Some(value) = response.headers().get(name) {
            builder = builder.header(name, value);
        }
    }
    let served = response
        .headers()
        .get("content-range")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| {
            let (bounds, total) = s.strip_prefix("bytes ")?.split_once('/')?;
            let (start, end) = bounds.split_once('-')?;
            Some((
                start.parse().ok()?,
                end.parse().ok()?,
                total.parse().unwrap_or(u64::MAX),
            ))
        });
    let expected = response.content_length();
    let is_mp4 = state.segments.lock().unwrap().is_empty()
        && (base.path().to_lowercase().ends_with(".mp4")
            || response
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok())
                .is_some_and(|s| s.contains("video/mp4")));
    let stream = track_media_stream(
        state,
        original_url,
        response.bytes_stream(),
        expected,
        status,
        served,
        is_mp4,
    );
    builder
        .header("cache-control", "no-store")
        .body(Body::from_stream(stream))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn track_media_stream<S, E>(
    state: Arc<MediaState>,
    url: String,
    stream: S,
    expected: Option<u64>,
    status: StatusCode,
    served: Option<(u64, u64, u64)>,
    is_mp4: bool,
) -> impl futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>>
where
    S: futures_util::Stream<Item = Result<axum::body::Bytes, E>> + Unpin,
{
    futures_util::stream::unfold(
        (stream, true, 0u64, state, url),
        move |(mut stream, mut good, mut received, state, url)| async move {
            match stream.next().await {
                Some(result) => {
                    good &= result.is_ok();
                    if let Ok(bytes) = &result {
                        if is_mp4 {
                            if let Some(total) = served.map(|(_, _, total)| total).or(expected) {
                                state.mp4_chunk(
                                    served
                                        .map(|(start, _, _)| start)
                                        .unwrap_or(0)
                                        .saturating_add(received),
                                    bytes.len() as u64,
                                    total,
                                );
                            }
                        }
                        received += bytes.len() as u64;
                    }
                    // Hyper may finish a fixed-length body without polling EOF.
                    if good
                        && expected.is_some_and(|length| received >= length)
                        && status.is_success()
                        && (status != StatusCode::PARTIAL_CONTENT || served.is_some())
                    {
                        state.complete_segment(&url, served);
                    }
                    Some((
                        result.map_err(|_| std::io::Error::other("读取媒体流失败")),
                        (stream, good, received, state, url),
                    ))
                }
                None => {
                    if good
                        && status.is_success()
                        && (status != StatusCode::PARTIAL_CONTENT || served.is_some())
                        && expected.is_none_or(|length| received >= length)
                    {
                        state.complete_segment(&url, served);
                    }
                    None
                }
            }
        },
    )
}

struct ProxyTask(tokio::task::JoinHandle<()>);
impl Drop for ProxyTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) async fn download(
    url: &str,
    token: &str,
    user_agent: &str,
    output: &Path,
    progress: Option<Arc<std::sync::atomic::AtomicU8>>,
) -> Result<(), String> {
    let state = Arc::new(MediaState {
        download_id: uuid::Uuid::new_v4(),
        urls: Mutex::new(HashMap::new()),
        token: token.into(),
        user_agent: user_agent.into(),
        progress: progress.clone(),
        segments: Mutex::new(HashMap::new()),
        mp4_bytes: Mutex::new((Vec::new(), 0)),
    });
    let download_id = state.download_id;
    tracing::info!(target:"app_replay_download",%download_id,output=%output.display(),"开始通过好学 App 下载回放");
    let path = state.register(Url::parse(url).map_err(|_| "视频地址无效")?)?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|_| "启动 Rust 媒体代理失败")?;
    let address = listener.local_addr().map_err(|_| "读取媒体代理地址失败")?;
    let router = Router::new()
        .route("/media/:id", get(media))
        .with_state(state);
    let _proxy = ProxyTask(tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    }));
    let mut command = tokio::process::Command::new("ffmpeg");
    command
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            &format!("http://{address}{path}"),
            "-c",
            "copy",
        ])
        .arg(output)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let result = tokio::time::timeout(Duration::from_secs(6 * 3600), command.output())
        .await
        .map_err(|_| "好学 App 媒体下载超时")?
        .map_err(|error| { tracing::error!(target:"app_replay_download",kind=?error.kind(),"failed to start ffmpeg"); "执行 ffmpeg 失败，请确认已安装并加入 PATH" })?;
    if !result.status.success()
        || tokio::fs::metadata(output)
            .await
            .map(|m| m.len() == 0)
            .unwrap_or(true)
    {
        tracing::error!(target:"app_replay_download",code=?result.status.code(),"ffmpeg did not produce a usable video");
        let stderr = String::from_utf8_lossy(&result.stderr);
        let urls = regex::Regex::new(r"https?://[^\s]+").unwrap();
        let message: String = urls
            .replace_all(&stderr, "[media-url]")
            .chars()
            .take(2000)
            .collect();
        tracing::error!(target:"app_replay_download",diagnostic=%message,"ffmpeg playback diagnostic");
        return Err(
            "好学 App 媒体下载失败，请检查网络和播放权限；详细原因见 app_replay_download 日志"
                .into(),
        );
    }
    if let Some(progress) = progress {
        progress.store(100, std::sync::atomic::Ordering::SeqCst);
    }
    tracing::info!(target:"app_replay_download",%download_id,percent=100,output=%output.display(),"好学回放下载及合并完成");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn mp4_byte_progress_does_not_count_overlapping_requests_twice() {
        let progress = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let state = MediaState {
            download_id: uuid::Uuid::new_v4(),
            urls: Mutex::new(HashMap::new()),
            token: String::new(),
            user_agent: String::new(),
            progress: Some(progress.clone()),
            segments: Mutex::new(HashMap::new()),
            mp4_bytes: Mutex::new((Vec::new(), 0)),
        };
        state.mp4_chunk(0, 50, 100);
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 50);
        state.mp4_chunk(25, 50, 100);
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 75);
        state.mp4_chunk(25, 50, 100);
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 75);
        state.mp4_chunk(75, 25, 100);
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 99);
    }
    #[tokio::test]
    async fn fixed_length_segment_updates_before_stream_eof() {
        let progress = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let state = Arc::new(MediaState {
            download_id: uuid::Uuid::new_v4(),
            urls: Mutex::new(HashMap::new()),
            token: String::new(),
            user_agent: String::new(),
            progress: Some(progress.clone()),
            segments: Mutex::new(HashMap::new()),
            mp4_bytes: Mutex::new((Vec::new(), 0)),
        });
        state.register_segments(
            &Url::parse("https://media.example/media.m3u8").unwrap(),
            "#EXTM3U\n#EXTINF:1,\na.ts\n#EXTINF:1,\nb.ts\n",
        );
        let input = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(
            axum::body::Bytes::from_static(b"1234"),
        )])
        .chain(futures_util::stream::pending());
        let stream = track_media_stream(
            state.clone(),
            "https://media.example/a.ts".into(),
            input,
            Some(4),
            StatusCode::OK,
            None,
            false,
        );
        futures_util::pin_mut!(stream);
        stream.next().await.unwrap().unwrap();
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 50);
        // No EOF was emitted; polling for it would wait forever.
        let input = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(
            axum::body::Bytes::from_static(b"12"),
        )]);
        let stream = track_media_stream(
            state,
            "https://media.example/b.ts".into(),
            input,
            Some(4),
            StatusCode::OK,
            None,
            false,
        );
        futures_util::pin_mut!(stream);
        while stream.next().await.is_some() {}
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 50);
    }
    #[test]
    fn hls_progress_counts_segments_once_and_signals_merge() {
        let progress = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let state = MediaState {
            download_id: uuid::Uuid::new_v4(),
            urls: Mutex::new(HashMap::new()),
            token: String::new(),
            user_agent: String::new(),
            progress: Some(progress.clone()),
            segments: Mutex::new(HashMap::new()),
            mp4_bytes: Mutex::new((Vec::new(), 0)),
        };
        let base = Url::parse("https://media.example/master.m3u8").unwrap();
        state.register_segments(
            &base,
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nchild.m3u8\n",
        );
        assert_eq!(state.segments.lock().unwrap().len(), 0);
        state.register_segments(&base, "#EXTM3U\n#EXTINF:10,\na.ts\n#EXTINF:10,\nb.ts\n");
        state.complete_segment("https://media.example/b.ts", None);
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 50);
        state.complete_segment("https://media.example/b.ts", None);
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 50);
        state.complete_segment("https://media.example/a.ts", None);
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 100);
    }
    #[test]
    fn byte_range_segments_require_the_complete_range() {
        let progress = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let state = MediaState {
            download_id: uuid::Uuid::new_v4(),
            urls: Mutex::new(HashMap::new()),
            token: String::new(),
            user_agent: String::new(),
            progress: Some(progress.clone()),
            segments: Mutex::new(HashMap::new()),
            mp4_bytes: Mutex::new((Vec::new(), 0)),
        };
        let base = Url::parse("https://media.example/media.m3u8").unwrap();
        state.register_segments(&base,"#EXTM3U\n#EXTINF:1,\n#EXT-X-BYTERANGE:10@0\nfile.mp4\n#EXTINF:1,\n#EXT-X-BYTERANGE:10\nfile.mp4\n");
        state.complete_segment("https://media.example/file.mp4", Some((0, 4, 20)));
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 0);
        state.complete_segment("https://media.example/file.mp4", Some((10, 19, 20)));
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 50);
        state.complete_segment("https://media.example/file.mp4", Some((0, 9, 20)));
        assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 100);
    }
    use super::*;
    #[tokio::test]
    async fn media_auth_uses_same_ua_and_strips_cookie_after_cross_host_redirect() {
        let state = MediaState {
            download_id: uuid::Uuid::new_v4(),
            urls: Mutex::new(HashMap::new()),
            token: "media-token".into(),
            user_agent: "random-pool-agent".into(),
            progress: None,
            segments: Mutex::new(HashMap::new()),
            mp4_bytes: Mutex::new((Vec::new(), 0)),
        };
        let client = reqwest::Client::new();
        let range = reqwest::header::HeaderValue::from_static("bytes=0-1023");
        let url = Url::parse("https://media.example/lecture.mp4").unwrap();
        let request = media_request(&client, &state, url.clone(), true, Some(&range))
            .build()
            .unwrap();
        assert_eq!(request.headers()["user-agent"], "random-pool-agent");
        assert_eq!(request.headers()["cookie"], "_token=media-token");
        assert_eq!(request.headers()["range"], "bytes=0-1023");
        assert!(!request.headers().contains_key("token"));
        let redirected = media_request(&client, &state, url, false, None)
            .build()
            .unwrap();
        assert!(!redirected.headers().contains_key("cookie"));
        assert_eq!(redirected.headers()["user-agent"], "random-pool-agent");
        assert!(
            validated_addresses(&Url::parse("https://127.0.0.1/lecture.mp4").unwrap())
                .await
                .is_err()
        );
    }
    #[test]
    fn nested_playlists_keys_and_maps_are_registered() {
        let state = MediaState {
            download_id: uuid::Uuid::new_v4(),
            urls: Mutex::new(HashMap::new()),
            token: "media-secret".into(),
            user_agent: "pool-agent".into(),
            progress: None,
            segments: Mutex::new(HashMap::new()),
            mp4_bytes: Mutex::new((Vec::new(), 0)),
        };
        let base = Url::parse("https://media.example/folder/master.m3u8").unwrap();
        let out=rewrite_playlist(&state,&base,"#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-STREAM-INF:BANDWIDTH=100\nchild.m3u8\nsegment.ts\n").unwrap();
        assert_eq!(out.matches("/media/").count(), 4);
        assert!(!out.contains("media-secret"));
        let urls = state.urls.lock().unwrap();
        assert_eq!(urls.len(), 4);
        assert!(urls
            .values()
            .any(|u| u.as_str() == "https://media.example/folder/key"));
    }
    #[test]
    fn private_addresses_and_non_https_are_rejected() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!global_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(global_ip("8.8.8.8".parse().unwrap()));
        assert!(check_media_url(&Url::parse("http://media.example/a.mp4").unwrap()).is_err());
    }
}

pub(super) async fn fetch_key(url: &str, token: &str, user_agent: &str) -> Result<String, String> {
    let state = MediaState {
        download_id: uuid::Uuid::new_v4(), urls: Mutex::new(HashMap::new()),
        token: token.into(), user_agent: user_agent.into(), progress: None,
        segments: Mutex::new(HashMap::new()), mp4_bytes: Mutex::new((Vec::new(), 0)),
    };
    let url = Url::parse(url).map_err(|e|e.to_string())?;
    check_media_url(&url)?;
    let bytes = upstream(&state, url, None).await?.bytes().await.map_err(|e|e.to_string())?;
    if bytes.len() != 16 { return Err(format!("密钥必须为16字节，实际为{}",bytes.len())); }
    Ok(super::prepared_media::encode_key(&bytes))
}
