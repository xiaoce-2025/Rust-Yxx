//! Candidate selection only. Teaching requests require a Windows UA; App clients do not.
use rand::{seq::SliceRandom, thread_rng};
use std::{collections::HashSet, fs::File, io::Read, path::Path};

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
pub fn choose(gzip: &str) -> Result<String, String> {
    choose_from(Path::new("./user_agents.txt"), Path::new(gzip), true)
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
