//! Boundary between authenticated discovery and independent video processing.
use super::{login_cache::LoginCache, pkulogin};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct PreparedMedia {
    pub url: String,
    pub enc_key: Option<String>,
}

pub fn encode_key(bytes: &[u8]) -> String {
    format!(
        "hex:{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

pub fn decode_key(key: &str) -> Result<Vec<u8>, String> {
    let bytes = if let Some(hex) = key.strip_prefix("hex:") {
        if hex.len() != 32 || !hex.is_ascii() {
            return Err("密钥必须为16字节".into());
        }
        (0..32)
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| "密钥hex编码无效".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        key.as_bytes().to_vec()
    };
    if bytes.len() != 16 {
        return Err("密钥必须为16字节".into());
    }
    Ok(bytes)
}

pub async fn public_bytes(url: &str, seconds: u64) -> Result<Vec<u8>, String> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(seconds.max(1)))
        .build()
        .map_err(|e| e.to_string())?
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| e.to_string())
}

pub fn public_bytes_blocking(url: &str) -> Result<Vec<u8>, String> {
    pkulogin::run_curl_bytes(&[
        "-sS".into(),
        "-L".into(),
        "--max-time".into(),
        "60".into(),
        url.into(),
    ])
}

fn next_playlist(url: &str, text: &str) -> Result<Option<String>, String> {
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err("响应不是m3u8播放列表".into());
    }
    let mut variant = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with("#EXT-X-STREAM-INF:") {
            variant = true;
        }
        if variant && !line.is_empty() && !line.starts_with('#') {
            return reqwest::Url::parse(url)
                .and_then(|base| base.join(line))
                .map(|u| Some(u.to_string()))
                .map_err(|e| e.to_string());
        }
    }
    if variant {
        return Err("主播放列表缺少媒体地址".into());
    }
    Ok(None)
}

pub async fn public_playlist(url: &str, seconds: u64) -> Result<(String, String), String> {
    let mut url = url.to_string();
    for _ in 0..6 {
        let text =
            String::from_utf8(public_bytes(&url, seconds).await?).map_err(|e| e.to_string())?;
        match next_playlist(&url, &text)? {
            Some(next) => url = next,
            None => return Ok((url, text)),
        }
    }
    Err("播放列表嵌套过深".into())
}

pub fn public_playlist_blocking(url: &str) -> Result<(String, String), String> {
    let mut url = url.to_string();
    for _ in 0..6 {
        let text = String::from_utf8(public_bytes_blocking(&url)?).map_err(|e| e.to_string())?;
        match next_playlist(&url, &text)? {
            Some(next) => url = next,
            None => return Ok((url, text)),
        }
    }
    Err("播放列表嵌套过深".into())
}

pub async fn prepare_teaching(
    url: &str,
    username: &str,
    password: &str,
    cache: &LoginCache,
    key: Option<&str>,
) -> Result<PreparedMedia, String> {
    if !url.contains(".m3u8") {
        return Ok(PreparedMedia {
            url: url.into(),
            enc_key: None,
        });
    }
    let key = if let Some(key) = key {
        decode_key(key)?;
        key.to_string()
    } else {
        let mut session =
            pkulogin::login_iaaa_and_get_cookies_with_cache(username, password, cache).await?;
        pkulogin::session_operation(&mut session, username, password, cache, |login| {
            let (_, key) = super::pkuvideo::fetch_m3u8_key_in_session(url, login)?;
            key
        })
        .await?
    };
    Ok(PreparedMedia {
        url: url.into(),
        enc_key: Some(key),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys_roundtrip_binary_and_legacy_ascii() {
        let bytes: Vec<u8> = (240..=255).collect();
        assert_eq!(decode_key(&encode_key(&bytes)).unwrap(), bytes);
        assert_eq!(decode_key("0123456789abcdef").unwrap(), b"0123456789abcdef");
        assert!(decode_key("short").is_err());
        assert!(decode_key("hex:zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
    }
    #[tokio::test]
    async fn supplied_key_never_logs_in_or_fetches_playlist() {
        let media = prepare_teaching(
            "https://invalid.example/video.m3u8",
            "",
            "",
            &LoginCache::new(60),
            Some("0123456789abcdef"),
        )
        .await
        .unwrap();
        assert!(media.enc_key.is_some());
    }

    #[tokio::test]
    async fn prepared_download_has_no_cookies_or_authentication() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/video.mp4", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 1024];
                let count = stream.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(!request.contains("cookie:"));
            assert!(!request.contains("authorization:"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nvideo",
                )
                .await
                .unwrap();
        });
        let dir = tempdir::TempDir::new("prepared-media").unwrap();
        let output = dir.path().join("video.mp4");
        super::super::pkuvideo::download_prepared(
            &PreparedMedia { url, enc_key: None },
            &output,
            1,
            5,
            None,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(output).unwrap(), b"video");
        server.await.unwrap();
    }

    #[test]
    fn key_uri_comes_from_manifest_without_filename_assumptions() {
        let manifest = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"/taskflow/harpocrates/rose/example\"\n#EXTINF:10,\nsegment.ts\n";
        assert_eq!(
            super::super::pkuvideo::key_uri(
                "https://resourcese.pku.edu.cn/play/0/playlist.m3u8",
                manifest
            )
            .unwrap(),
            "https://resourcese.pku.edu.cn/taskflow/harpocrates/rose/example"
        );
    }

    #[test]
    fn master_playlist_resolves_relative_variant() {
        assert_eq!(
            next_playlist(
                "https://media.example/hls/master.m3u8",
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n../video/playlist.m3u8"
            )
            .unwrap()
            .unwrap(),
            "https://media.example/video/playlist.m3u8"
        );
        assert!(next_playlist("https://media.example/hls/", "<html>login</html>").is_err());
    }
}
