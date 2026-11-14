//! Persistent teaching UA exploration/success pools; independent App UA selection.
use rand::{seq::SliceRandom, thread_rng};
use std::{
    collections::HashSet,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

static POOL_LOCK: Mutex<()> = Mutex::new(());

/// Teaching login explores the existing pools without replacement. Successful
/// UAs are retained separately and sampled without consuming them.
pub(crate) struct TeachingUaPool {
    pub plain: PathBuf,
    pub gzip: PathBuf,
    pub successful: PathBuf,
}
impl Default for TeachingUaPool {
    fn default() -> Self {
        Self {
            plain: "./user_agents.txt".into(),
            gzip: "./user_agents.txt.gz".into(),
            successful: "./data/teaching_success_user_agents.txt".into(),
        }
    }
}
fn read_pool(path: &Path, compressed: bool) -> Result<Option<String>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("读取 UA 池失败：{e}")),
    };
    let mut reader: Box<dyn Read> = if compressed {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut text = String::new();
    reader
        .read_to_string(&mut text)
        .map_err(|e| format!("读取 UA 池失败：{e}"))?;
    Ok(Some(text))
}
fn ua_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|s| s.trim().trim_start_matches('\u{feff}').trim().to_owned())
        .collect()
}
fn replace_pool(path: &Path, text: &str, compressed: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| format!("创建 UA 池目录失败：{e}"))?;
    let dir = tempdir::TempDir::new_in(parent, "ua-update").map_err(|e| e.to_string())?;
    let pending = dir.path().join("pool");
    let file = File::create(&pending).map_err(|e| e.to_string())?;
    if compressed {
        let mut zip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        zip.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        zip.finish()
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
    } else {
        let mut file = file;
        file.write_all(text.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
    }
    std::fs::rename(pending, path).map_err(|e| format!("更新 UA 池失败：{e}"))
}
impl TeachingUaPool {
    pub fn take_candidate(&self) -> Result<Option<String>, String> {
        let _guard = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let plain = read_pool(&self.plain, false)?;
        let gzip = read_pool(&self.gzip, true)?;
        let candidates = |text: &str| {
            ua_lines(text)
                .into_iter()
                .filter(|ua| is_windows(ua))
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        };
        let mut available = candidates(plain.as_deref().unwrap_or(""));
        if available.is_empty() {
            available = candidates(gzip.as_deref().unwrap_or(""));
        }
        let Some(ua) = available.choose(&mut thread_rng()).cloned() else {
            return Ok(None);
        };
        // Remove every duplicate from BOTH sources so gzip fallback cannot resurrect it.
        for (path, text, compressed) in [(&self.plain, plain, false), (&self.gzip, gzip, true)] {
            if let Some(text) = text {
                let lines = ua_lines(&text);
                if lines.iter().any(|line| line == &ua) {
                    let remaining = lines
                        .into_iter()
                        .filter(|line| line != &ua)
                        .collect::<Vec<_>>()
                        .join("\n");
                    replace_pool(path, &format!("{remaining}\n"), compressed)?;
                }
            }
        }
        tracing::info!(user_agent=%ua, pool="candidate", "已随机抽取并移除 UA（完整值）");
        Ok(Some(ua))
    }
    pub fn choose_successful(&self) -> Result<Option<String>, String> {
        let _guard = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let text = read_pool(&self.successful, false)?.unwrap_or_default();
        let available = ua_lines(&text)
            .into_iter()
            .filter(|ua| is_windows(ua))
            .collect::<Vec<_>>();
        let ua = available.choose(&mut thread_rng()).cloned();
        if let Some(ua) = &ua {
            tracing::info!(user_agent=%ua, pool="successful", "复用成功 UA，保留池中记录（完整值）");
        }
        Ok(ua)
    }
    pub fn record_success(&self, ua: &str) -> Result<(), String> {
        ensure_teaching(ua)?;
        let _guard = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let text = read_pool(&self.successful, false)?.unwrap_or_default();
        let mut lines = ua_lines(&text)
            .into_iter()
            .filter(|ua| is_windows(ua))
            .collect::<Vec<_>>();
        if !lines.iter().any(|line| line == ua) {
            lines.push(ua.into());
            replace_pool(&self.successful, &format!("{}\n", lines.join("\n")), false)?;
        }
        tracing::info!(user_agent=%ua, "已记录通过教学网完整登录验证的 UA（完整值）");
        Ok(())
    }
}

pub fn valid_header(ua: &str) -> bool {
    !ua.trim().is_empty() && !ua.chars().any(char::is_control)
}
pub fn is_windows(ua: &str) -> bool {
    valid_header(ua)
        && ua
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| word.eq_ignore_ascii_case("windows"))
}
pub fn ensure_teaching(ua: &str) -> Result<(), String> {
    if is_windows(ua) {
        Ok(())
    } else {
        Err("教学网仅允许带 Windows 标签的 User-Agent，请重新登录".into())
    }
}
pub fn choose_app(gzip: &str) -> Result<String, String> {
    choose_from(Path::new("./user_agents.txt"), Path::new(gzip), false)
}
fn choose_from(plain: &Path, gzip: &Path, windows_only: bool) -> Result<String, String> {
    let mut seen = HashSet::new();
    let mut filter = |content: String| {
        content
            .lines()
            .map(|line| {
                line.trim()
                    .trim_start_matches('\u{feff}')
                    .trim()
                    .to_string()
            })
            .filter(|ua| {
                valid_header(ua) && (!windows_only || is_windows(ua)) && seen.insert(ua.clone())
            })
            .collect::<Vec<_>>()
    };
    let text = match std::fs::read_to_string(plain) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.to_string()),
    };
    let mut source = plain;
    let mut candidates = filter(text);
    if candidates.is_empty() {
        source = gzip;
        match File::open(gzip) {
            Ok(file) => {
                let mut text = String::new();
                flate2::read::GzDecoder::new(file)
                    .read_to_string(&mut text)
                    .map_err(|e| e.to_string())?;
                candidates = filter(text);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let ua = candidates
        .choose(&mut thread_rng())
        .cloned()
        .ok_or_else(|| {
            if windows_only {
                "没有可用的 Windows User-Agent，请补充 user_agents.txt 或 user_agents.txt.gz"
                    .to_string()
            } else {
                "没有可用的 App User-Agent，请补充 UA 候选文件".into()
            }
        })?;
    tracing::info!(user_agent=%ua,windows_only,available_count=candidates.len(),source=%source.display(),"已选择 User-Agent（完整值）");
    Ok(ua)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write, sync::Mutex};
    #[test]
    fn teaching_pool_consumes_duplicates_in_plain_and_gzip_and_persists_success() {
        let dir = tempdir::TempDir::new("ua-pools").unwrap();
        let pool = TeachingUaPool {
            plain: dir.path().join("ua.txt"),
            gzip: dir.path().join("ua.gz"),
            successful: dir.path().join("success.txt"),
        };
        fs::write(
            &pool.plain,
            "\nWindows candidate-A\nWindows candidate-A\nAndroid\n",
        )
        .unwrap();
        replace_pool(
            &pool.gzip,
            "Windows candidate-A\nWindows candidate-B\n\n",
            true,
        )
        .unwrap();
        assert_eq!(
            pool.take_candidate().unwrap().as_deref(),
            Some("Windows candidate-A")
        );
        assert!(!read_pool(&pool.plain, false)
            .unwrap()
            .unwrap()
            .contains("candidate-A"));
        assert!(!read_pool(&pool.gzip, true)
            .unwrap()
            .unwrap()
            .contains("candidate-A"));
        pool.record_success("Windows candidate-A").unwrap();
        pool.record_success("Windows candidate-A").unwrap();
        assert_eq!(
            ua_lines(&fs::read_to_string(&pool.successful).unwrap()).len(),
            1
        );
        for _ in 0..3 {
            assert_eq!(
                pool.choose_successful().unwrap().as_deref(),
                Some("Windows candidate-A")
            );
        }
        assert_eq!(
            pool.take_candidate().unwrap().as_deref(),
            Some("Windows candidate-B")
        );
        assert!(pool.take_candidate().unwrap().is_none());
        assert!(fs::read_to_string(&pool.plain).unwrap().contains("Android"));
    }

    #[test]
    fn concurrent_teaching_logins_cannot_consume_the_same_candidate() {
        let dir = tempdir::TempDir::new("ua-concurrent").unwrap();
        let pool = std::sync::Arc::new(TeachingUaPool {
            plain: dir.path().join("ua.txt"),
            gzip: dir.path().join("absent.gz"),
            successful: dir.path().join("success.txt"),
        });
        fs::write(
            &pool.plain,
            (0..8)
                .map(|i| format!("Windows candidate-{i}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let threads = (0..8)
            .map(|_| {
                let pool = pool.clone();
                std::thread::spawn(move || pool.take_candidate().unwrap().unwrap())
            })
            .collect::<Vec<_>>();
        let selected = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect::<HashSet<_>>();
        assert_eq!(selected.len(), 8);
        assert!(pool.take_candidate().unwrap().is_none());
    }
    #[test]
    fn teaching_uses_windows_and_app_accepts_android() {
        let dir = tempdir::TempDir::new("ua-platform").unwrap();
        let plain = dir.path().join("ua.txt");
        let gzip = dir.path().join("none.gz");
        let windows = "Mozilla/5.0 (Windows NT 10.0; Win64; x64)";
        std::fs::write(&plain,format!("\u{feff}Mozilla/5.0 (Linux; Android 14)\n{windows}\nMozilla/5.0 (Macintosh; Intel Mac OS X)\n")).unwrap();
        for _ in 0..10 {
            assert_eq!(choose_from(&plain, &gzip, true).unwrap(), windows);
        }
        std::fs::write(&plain, "Mozilla/5.0 (Linux; Android 14)").unwrap();
        assert!(choose_from(&plain, &gzip, true).is_err());
        assert!(choose_from(&plain, &gzip, false)
            .unwrap()
            .contains("Android"));
        assert!(!is_windows("NotWindows"));
        assert!(is_windows("Mozilla/5.0 (windows NT 10.0)"));
        assert!(!is_windows("Windows\r\ninvalid"));
    }
    #[test]
    fn gzip_fallback_applies_platform_filter_and_ignores_old_blacklist() {
        let dir = tempdir::TempDir::new("ua-gzip").unwrap();
        let plain = dir.path().join("ua.txt");
        let compressed = dir.path().join("ua.gz");
        std::fs::write(&plain, "Android mobile").unwrap();
        let mut zip = flate2::write::GzEncoder::new(
            File::create(&compressed).unwrap(),
            flate2::Compression::default(),
        );
        zip.write_all(b"Macintosh\nWindows NT 10.0 test\n").unwrap();
        zip.finish().unwrap();
        std::fs::write(
            dir.path().join("failed_user_agents.txt"),
            "Windows NT 10.0 test",
        )
        .unwrap();
        assert_eq!(
            choose_from(&plain, &compressed, true).unwrap(),
            "Windows NT 10.0 test"
        );
    }
    #[test]
    fn info_log_contains_the_entire_selected_user_agent() {
        #[derive(Clone)]
        struct Capture(std::sync::Arc<Mutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let dir = tempdir::TempDir::new("ua-log").unwrap();
        let plain = dir.path().join("agents.txt");
        let ua = format!(
            "Mozilla/5.0 (Windows NT 10.0) {} end-of-full-ua",
            "long-browser-details ".repeat(20)
        );
        fs::write(&plain, &ua).unwrap();
        let bytes = std::sync::Arc::new(Mutex::new(Vec::new()));
        let writer = Capture(bytes.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            assert_eq!(
                choose_from(&plain, &dir.path().join("missing.gz"), true).unwrap(),
                ua
            );
        });
        let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(log.contains("INFO"), "captured log: {log:?}");
        assert!(log.contains(&ua));
        assert!(log.contains("user_agent="));
    }
}
