//! Read-only random UA selection shared by teaching and App clients.
use rand::{seq::SliceRandom, thread_rng};
use std::{fs::File, io::Read, path::Path};

pub fn valid_header(ua: &str) -> bool {
    !ua.trim().is_empty() && !ua.chars().any(char::is_control)
}
pub fn choose(gzip: &str) -> Result<String, String> {
    choose_from(Path::new("./user_agents.txt"), Path::new(gzip))
}
pub fn choose_app(gzip: &str) -> Result<String, String> {
    choose_from(Path::new("./user_agents.txt"), Path::new(gzip))
}
fn choose_from(plain: &Path, gzip: &Path) -> Result<String, String> {
    let filter = |content: String| {
        content
            .lines()
            .map(|line| {
                line.trim()
                    .trim_start_matches('\u{feff}')
                    .trim()
                    .to_string()
            })
            .filter(|ua| {
                valid_header(ua)
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
        .ok_or_else(|| "没有可用的 User-Agent，请补充 user_agents.txt 或 user_agents.txt.gz".to_string())?;
    tracing::info!(user_agent=%ua,available_count=candidates.len(),source=%source.display(),"已选择 User-Agent（完整值）");
    Ok(ua)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write, sync::Mutex};
    #[test]
    fn selection_accepts_all_platforms_without_modifying_pools() {
        let dir = tempdir::TempDir::new("ua-readonly").unwrap();
        let plain = dir.path().join("ua.txt");
        let gzip = dir.path().join("missing.gz");
        for ua in ["Windows NT 10.0", "Android mobile", "Macintosh"] {
            let content = format!("\u{feff}{ua}\n{ua}\n\n");
            fs::write(&plain, &content).unwrap();
            for _ in 0..3 { assert_eq!(choose_from(&plain, &gzip).unwrap(), ua); }
            assert_eq!(fs::read_to_string(&plain).unwrap(), content);
        }
        assert!(!valid_header("Windows\r\ninvalid"));
        assert!(!valid_header(" "));
    }
    #[test]
    fn gzip_fallback_is_readonly_and_ignores_login_history() {
        let dir = tempdir::TempDir::new("ua-gzip").unwrap();
        let plain = dir.path().join("ua.txt");
        let gzip = dir.path().join("ua.gz");
        fs::write(&plain, "\n \n").unwrap();
        let mut zip = flate2::write::GzEncoder::new(File::create(&gzip).unwrap(), flate2::Compression::default());
        zip.write_all(b"Android mobile\n").unwrap();
        zip.finish().unwrap();
        fs::write(dir.path().join("failed_user_agents.txt"), "Android mobile").unwrap();
        fs::write(dir.path().join("teaching_success_user_agents.txt"), "Windows successful").unwrap();
        let original = fs::read(&gzip).unwrap();
        assert_eq!(choose_from(&plain, &gzip).unwrap(), "Android mobile");
        assert_eq!(fs::read(&gzip).unwrap(), original);
        fs::remove_file(&gzip).unwrap();
        assert!(choose_from(&plain, &gzip).is_err());
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
                choose_from(&plain, &dir.path().join("missing.gz")).unwrap(),
                ua
            );
        });
        let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(log.contains("INFO"), "captured log: {log:?}");
        assert!(log.contains(&ua));
        assert!(log.contains("user_agent="));
    }
}
