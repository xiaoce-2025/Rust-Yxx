//! Persistent failed-UA quarantine shared by login and candidate selection.
use rand::{seq::SliceRandom, thread_rng};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};
#[cfg(not(test))]
const FAILED_FILE: &str = "./data/failed_user_agents.txt";
static FILE_LOCK: Mutex<()> = Mutex::new(());

pub fn failed_file() -> PathBuf {
    #[cfg(not(test))]
    {
        PathBuf::from(FAILED_FILE)
    }
    #[cfg(test)]
    {
        // Network/session unit tests must never modify the deployed service's quarantine file.
        static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        PATH.get_or_init(|| {
            std::env::temp_dir().join(format!(
                "rust-yxx-failed-ua-tests-{}.txt",
                uuid::Uuid::new_v4()
            ))
        })
        .clone()
    }
}
fn lines(path: &Path) -> Result<Vec<String>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("读取 {} 失败: {error}", path.display())),
    };
    BufReader::new(file)
        .lines()
        .map(|line| {
            line.map(|s| s.trim().to_string())
                .map_err(|e| e.to_string())
        })
        .filter(|line| !matches!(line,Ok(value) if value.is_empty()))
        .collect()
}
fn record_at(path: &Path, ua: &str) -> Result<bool, String> {
    let ua = ua.trim();
    if ua.is_empty() || ua.chars().any(char::is_control) {
        return Err("不能记录空白或含控制字符的User-Agent".into());
    }
    let _lock = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if lines(path)?.iter().any(|value| value == ua) {
        return Ok(false);
    }
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    // Keep the new entry separate even if a manually edited file lacks its final newline.
    let prefix = if file.metadata().map_err(|e| e.to_string())?.len() > 0 {
        file.seek(SeekFrom::End(-1)).map_err(|e| e.to_string())?;
        let mut last = [0];
        file.read_exact(&mut last).map_err(|e| e.to_string())?;
        if last[0] == b'\n' {
            ""
        } else {
            "\n"
        }
    } else {
        ""
    };
    file.write_all(format!("{prefix}{ua}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    Ok(true)
}
pub fn record_failure(ua: &str, stage: &str) {
    let path = failed_file();
    match record_at(&path, ua) {
        Ok(added) => {
            tracing::warn!(user_agent=%ua,failure_stage=%stage,failed_file=%path.display(),new_entry=added,"请求失败，User-Agent 已列入失败名单")
        }
        Err(error) => {
            tracing::error!(user_agent=%ua,failure_stage=%stage,failed_file=%path.display(),%error,"记录失败 User-Agent 失败")
        }
    }
}
pub fn is_failed(ua: &str) -> Result<bool, String> {
    if ua.trim().is_empty() || ua.chars().any(char::is_control) {
        return Ok(true);
    }
    let _lock = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    Ok(lines(&failed_file())?
        .iter()
        .any(|value| value == ua.trim()))
}
pub fn choose(gzip: &str) -> Result<String, String> {
    choose_from(
        Path::new("./user_agents.txt"),
        Path::new(gzip),
        &failed_file(),
    )
}
fn choose_from(plain: &Path, gzip: &Path, failed: &Path) -> Result<String, String> {
    let _lock = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let excluded: HashSet<_> = lines(failed)?.into_iter().collect();
    let mut seen = HashSet::new();
    let filter = |values: Vec<String>, seen: &mut HashSet<String>| {
        values
            .into_iter()
            .filter(|value| {
                !value.is_empty()
                    && !value.chars().any(char::is_control)
                    && !excluded.contains(value)
                    && seen.insert(value.clone())
            })
            .collect::<Vec<_>>()
    };
    let plain_values = lines(plain)?;
    let mut source = plain;
    let mut candidates = filter(plain_values, &mut seen);
    if candidates.is_empty() {
        source = gzip;
        match File::open(gzip) {
            Ok(file) => {
                let mut content = String::new();
                flate2::read::GzDecoder::new(file)
                    .read_to_string(&mut content)
                    .map_err(|e| format!("读取UA压缩包失败: {e}"))?;
                candidates = filter(
                    content.lines().map(str::trim).map(str::to_string).collect(),
                    &mut seen,
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("打开 {} 失败: {error}", gzip.display())),
        }
    }
    let ua=candidates.choose(&mut thread_rng()).cloned().ok_or_else(||format!(
        "过滤失败名单后没有可用 User-Agent，请补充 user_agents.txt/user_agents.txt.gz 或检查 {}",failed.display()))?;
    tracing::info!(user_agent=%ua,available_count=candidates.len(),failed_count=excluded.len(),source=%source.display(),failed_file=%failed.display(),"已选择 User-Agent（完整值）");
    Ok(ua)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gzip(path: &Path, text: &str) {
        let mut writer = flate2::write::GzEncoder::new(
            File::create(path).unwrap(),
            flate2::Compression::default(),
        );
        writer.write_all(text.as_bytes()).unwrap();
        writer.finish().unwrap();
    }
    #[test]
    fn persisted_failures_are_deduplicated_and_filtered_after_reload() {
        let dir = tempdir::TempDir::new("ua-filter").unwrap();
        let plain = dir.path().join("agents.txt");
        let compressed = dir.path().join("agents.gz");
        let failed = dir.path().join("data/failed.txt");
        fs::write(&plain, "bad-agent\ngood-agent\nbad-agent\n\n").unwrap();
        assert!(record_at(&failed, "bad-agent").unwrap());
        assert!(!record_at(&failed, "bad-agent").unwrap());
        assert_eq!(lines(&failed).unwrap(), vec!["bad-agent"]);
        for _ in 0..10 {
            assert_eq!(
                choose_from(&plain, &compressed, &failed).unwrap(),
                "good-agent"
            );
        }
        assert!(record_at(&failed, "good-agent").unwrap());
        assert!(choose_from(&plain, &compressed, &failed)
            .unwrap_err()
            .contains("没有可用"));
        fs::remove_file(&failed).unwrap();
        assert!(choose_from(&plain, &compressed, &failed).is_ok());
    }
    #[test]
    fn filtered_plain_pool_falls_back_to_filtered_gzip_without_reviving_failed_ua() {
        let dir = tempdir::TempDir::new("ua-gzip").unwrap();
        let plain = dir.path().join("agents.txt");
        let compressed = dir.path().join("agents.gz");
        let failed = dir.path().join("failed.txt");
        fs::write(&plain, "bad-agent\n").unwrap();
        gzip(&compressed, "bad-agent\nzip-good\nzip-good\n");
        fs::write(&failed, "bad-agent").unwrap();
        assert_eq!(
            choose_from(&plain, &compressed, &failed).unwrap(),
            "zip-good"
        );
        record_at(&failed, "zip-good").unwrap();
        assert_eq!(lines(&failed).unwrap(), vec!["bad-agent", "zip-good"]);
        assert!(choose_from(&plain, &compressed, &failed).is_err());
    }
    #[test]
    fn concurrent_failures_do_not_duplicate_or_lose_entries() {
        let dir = tempdir::TempDir::new("ua-concurrency").unwrap();
        let failed = dir.path().join("failed.txt");
        std::thread::scope(|scope| {
            for i in 0..20 {
                let path = &failed;
                scope.spawn(move || record_at(path, &format!("ua-{}", i % 5)).unwrap());
            }
        });
        let entries = lines(&failed).unwrap();
        assert_eq!(entries.len(), 5);
        assert_eq!(entries.into_iter().collect::<HashSet<_>>().len(), 5);
        assert!(record_at(&failed, "invalid\r\nagent").is_err());
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
            "Mozilla/5.0 {} end-of-full-ua",
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
            assert_eq!(
                choose_from(
                    &plain,
                    &dir.path().join("missing.gz"),
                    &dir.path().join("failed.txt")
                )
                .unwrap(),
                ua
            );
        });
        let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(log.contains("INFO"));
        assert!(log.contains(&ua));
        assert!(log.contains("user_agent="));
    }
}
