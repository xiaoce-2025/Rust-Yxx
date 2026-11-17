//! Single-instance URL cache. A durable publication journal bridges SQLite and files.
use super::{FileStore, StoreError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub fn tex_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn reuse_cached_tex(regenerate: bool, initial: Option<&str>, global: Option<&str>) -> bool {
    match global {
        None => false,
        Some(hash) => !regenerate || initial.is_some_and(|base| base != hash),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalTexEntry {
    pub url_key: String,
    pub tex_path: String,
    pub tex_sha256: String,
    pub revision: i64,
    pub source_job_id: String,
    pub created_at: String,
    pub updated_at: String,
}

pub(crate) fn valid_tex(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .is_ok_and(|s| !s.trim().is_empty() && s.contains('\\') && !s.contains('\0'))
}

/// Rust rename replaces a destination atomically on Unix and Windows. Never unlink first.
pub(crate) fn atomic_tex(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let parent = path.parent().ok_or("文件目录不存在")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let staging = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::File::create(&staging).map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        std::fs::rename(&staging, path).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        std::fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(staging);
    }
    result
}

impl FileStore {
    pub(super) async fn init_tex_cache(&self) -> Result<(), StoreError> {
        for sql in [
            "CREATE TABLE IF NOT EXISTS global_tex_cache (url_key TEXT PRIMARY KEY, tex_path TEXT NOT NULL, tex_sha256 TEXT NOT NULL, video_duration_seconds INTEGER, revision INTEGER NOT NULL DEFAULT 1, source_job_id TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, last_hit_at TEXT, hit_count INTEGER NOT NULL DEFAULT 0)",
            "CREATE TABLE IF NOT EXISTS global_cache_course_observations (job_id TEXT PRIMARY KEY, url_key TEXT NOT NULL, course_name TEXT, course_date TEXT, course_id TEXT, lesson_id TEXT, observed_at TEXT NOT NULL)",
            "CREATE INDEX IF NOT EXISTS idx_cache_observations_url ON global_cache_course_observations(url_key, course_date)",
            "CREATE TABLE IF NOT EXISTS global_tex_publications (url_key TEXT PRIMARY KEY, metadata TEXT NOT NULL, tex BLOB NOT NULL)",
        ] { sqlx::query(sql).execute(&self.db).await?; }
        let columns = sqlx::query("PRAGMA table_info(global_tex_cache)")
            .fetch_all(&self.db)
            .await?;
        if !columns
            .iter()
            .any(|row| row.get::<String, _>("name") == "video_duration_seconds")
        {
            sqlx::query("ALTER TABLE global_tex_cache ADD COLUMN video_duration_seconds INTEGER")
                .execute(&self.db)
                .await?;
        }
        self.recover_tex_publications().await?;
        Ok(())
    }

    fn cache_path(&self, key: &str) -> Result<PathBuf, String> {
        if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("缓存键无效".into());
        }
        Ok(self
            .data_dir
            .join("global_tex_cache")
            .join(&key[..2])
            .join(format!("{key}.tex")))
    }

    async fn commit_publication(
        &self,
        entry: &GlobalTexEntry,
        bytes: &[u8],
    ) -> Result<(), StoreError> {
        if tex_hash(bytes) != entry.tex_sha256 || !valid_tex(bytes) {
            return Err("缓存发布日志校验失败".into());
        }
        let path = self.cache_path(&entry.url_key)?;
        atomic_tex(&path, bytes)?;
        let mut tx = self.db.begin().await?;
        sqlx::query("INSERT INTO global_tex_cache (url_key,tex_path,tex_sha256,revision,source_job_id,created_at,updated_at) VALUES (?,?,?,?,?,?,?) ON CONFLICT(url_key) DO UPDATE SET tex_path=excluded.tex_path,tex_sha256=excluded.tex_sha256,revision=excluded.revision,source_job_id=excluded.source_job_id,updated_at=excluded.updated_at")
            .bind(&entry.url_key).bind(&entry.tex_path).bind(&entry.tex_sha256).bind(entry.revision)
            .bind(&entry.source_job_id).bind(&entry.created_at).bind(&entry.updated_at).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM global_tex_publications WHERE url_key=?")
            .bind(&entry.url_key)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn recover_tex_publications(&self) -> Result<(), StoreError> {
        let _guard = self.tex_cache_lock.lock().await;
        let rows = sqlx::query("SELECT metadata,tex FROM global_tex_publications")
            .fetch_all(&self.db)
            .await?;
        for row in rows {
            let entry: GlobalTexEntry = serde_json::from_str(row.get("metadata"))?;
            self.commit_publication(&entry, row.get("tex")).await?;
        }
        Ok(())
    }

    pub async fn verified_tex(
        &self,
        key: &str,
    ) -> Result<Option<(GlobalTexEntry, Vec<u8>)>, StoreError> {
        let _guard = self.tex_cache_lock.lock().await;
        // Finish an interrupted publication before trusting its old metadata.
        let pending =
            sqlx::query("SELECT metadata,tex FROM global_tex_publications WHERE url_key=?")
                .bind(key)
                .fetch_optional(&self.db)
                .await?;
        if let Some(row) = pending {
            let entry = serde_json::from_str(row.get("metadata"))?;
            self.commit_publication(&entry, row.get("tex")).await?;
        }
        let row = sqlx::query("SELECT * FROM global_tex_cache WHERE url_key=?")
            .bind(key)
            .fetch_optional(&self.db)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let entry = GlobalTexEntry {
            url_key: key.into(),
            tex_path: row.get("tex_path"),
            tex_sha256: row.get("tex_sha256"),
            revision: row.get("revision"),
            source_job_id: row.get("source_job_id"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        };
        match tokio::fs::read(self.cache_path(key)?).await {
            Ok(bytes) if valid_tex(&bytes) && tex_hash(&bytes) == entry.tex_sha256 => {
                Ok(Some((entry, bytes)))
            }
            _ => {
                tracing::warn!(url_key = key, "全局 TeX 缓存校验失败，按未命中处理");
                Ok(None)
            }
        }
    }

    pub async fn publish_tex(
        &self,
        key: &str,
        bytes: &[u8],
        job: Uuid,
    ) -> Result<GlobalTexEntry, StoreError> {
        let _guard = self.tex_cache_lock.lock().await;
        if !valid_tex(bytes) {
            return Err("生成的 TeX 内容无效".into());
        }
        let old = sqlx::query("SELECT revision,created_at FROM global_tex_cache WHERE url_key=?")
            .bind(key)
            .fetch_optional(&self.db)
            .await?;
        let now = chrono::Utc::now().to_rfc3339();
        let entry = GlobalTexEntry {
            url_key: key.into(),
            tex_path: self.cache_path(key)?.to_string_lossy().into(),
            tex_sha256: tex_hash(bytes),
            revision: old.as_ref().map_or(1, |r| r.get::<i64, _>("revision") + 1),
            source_job_id: job.to_string(),
            created_at: old.map_or(now.clone(), |r| r.get("created_at")),
            updated_at: now,
        };
        sqlx::query("INSERT INTO global_tex_publications (url_key,metadata,tex) VALUES (?,?,?)")
            .bind(key)
            .bind(serde_json::to_string(&entry)?)
            .bind(bytes)
            .execute(&self.db)
            .await?;
        self.commit_publication(&entry, bytes).await?;
        tracing::info!(url_key = key, revision = entry.revision, "全局 TeX 已发布");
        Ok(entry)
    }

    pub async fn record_tex_hit(&self, key: &str) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE global_tex_cache SET hit_count=hit_count+1,last_hit_at=? WHERE url_key=?",
        )
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(key)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn tex_cache_records(
        &self,
        search: &str,
        page: u32,
    ) -> Result<serde_json::Value, StoreError> {
        let pattern = format!("%{search}%");
        let filter = " FROM global_tex_cache c WHERE c.url_key LIKE ? OR EXISTS (SELECT 1 FROM global_cache_course_observations o WHERE o.url_key=c.url_key AND (o.course_name LIKE ? OR o.course_date LIKE ?))";
        let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*){filter}"))
            .bind(&pattern)
            .bind(&pattern)
            .bind(&pattern)
            .fetch_one(&self.db)
            .await?;
        let sql = format!("SELECT json_object('url_key',c.url_key,'tex_sha256',c.tex_sha256,'revision',c.revision,'source_job_id',c.source_job_id,'created_at',c.created_at,'updated_at',c.updated_at,'last_hit_at',c.last_hit_at,'hit_count',c.hit_count,'observations',(SELECT COUNT(*) FROM global_cache_course_observations o WHERE o.url_key=c.url_key)) {filter} ORDER BY c.updated_at DESC,c.url_key LIMIT 50 OFFSET ?");
        let rows: Vec<String> = sqlx::query_scalar(&sql)
            .bind(&pattern)
            .bind(&pattern)
            .bind(&pattern)
            .bind(i64::from(page.max(1) - 1) * 50)
            .fetch_all(&self.db)
            .await?;
        let records: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| serde_json::from_str(r))
            .collect::<Result<_, _>>()?;
        Ok(serde_json::json!({"records":records,"total":total,"page":page.max(1),"page_size":50}))
    }

    pub async fn tex_cache_observations(&self, key: &str) -> Result<serde_json::Value, StoreError> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT json_object('job_id',job_id,'course_name',course_name,'course_date',course_date,'course_id',course_id,'lesson_id',lesson_id,'observed_at',observed_at) FROM global_cache_course_observations WHERE url_key=? ORDER BY course_date DESC,observed_at DESC")
            .bind(key).fetch_all(&self.db).await?;
        let mut records = Vec::new();
        for row in rows {
            let mut value: serde_json::Value = serde_json::from_str(&row)?;
            if let Some(job) = value["job_id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                if let Some(job) = self.get_pkuvideo_job(job).await {
                    value["result_source"] = serde_json::to_value(job.result_source)?;
                    value["status"] = serde_json::to_value(job.status)?;
                }
            }
            records.push(value);
        }
        Ok(serde_json::json!(records))
    }

    pub async fn observe_tex_course(
        &self,
        job: &crate::services::pkuvideo::PkuVideoJob,
    ) -> Result<(), StoreError> {
        sqlx::query("INSERT OR IGNORE INTO global_cache_course_observations (job_id,url_key,course_name,course_date,course_id,lesson_id,observed_at) VALUES (?,?,?,?,?,?,?)")
            .bind(job.id.to_string()).bind(&job.url_key).bind(&job.course_name).bind(&job.course_date)
            .bind(&job.course_id).bind(&job.lesson_id).bind(job.created_at.to_rfc3339()).execute(&self.db).await?;
        Ok(())
    }

    pub fn user_tex_path(&self, user: Uuid, job: Uuid) -> PathBuf {
        self.data_dir
            .join("user_notes")
            .join(user.to_string())
            .join(format!("{job}.tex"))
    }

    pub async fn standalone_initial_hash(
        &self,
        user: Uuid,
        key: &str,
    ) -> Result<Option<String>, String> {
        self.load_sidecar(
            &self
                .data_dir
                .join("user_notes")
                .join(user.to_string())
                .join(format!("{key}.index.json")),
        )
        .await
    }

    pub async fn set_standalone_initial_hash(
        &self,
        user: Uuid,
        key: &str,
        hash: &str,
    ) -> Result<(), String> {
        self.save_sidecar(
            &self
                .data_dir
                .join("user_notes")
                .join(user.to_string())
                .join(format!("{key}.index.json")),
            &serde_json::to_string(hash).map_err(|e| e.to_string())?,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn regeneration_matrix_uses_install_baseline() {
        for (regenerate, base, global, expected) in [
            (false, None, None, false),
            (false, None, Some("H1"), true),
            (true, Some("H1"), Some("H1"), false),
            (true, Some("H1"), Some("H2"), true),
            (true, None, Some("H2"), false),
            (true, Some("H1"), None, false),
        ] {
            assert_eq!(reuse_cached_tex(regenerate, base, global), expected);
        }
    }
    #[tokio::test]
    async fn publication_integrity_revision_and_recovery() {
        let dir = tempdir::TempDir::new("tex-cache").unwrap();
        let store = FileStore::new(dir.path().into()).await.unwrap();
        let key = tex_hash(b"https://example/video?token=unaltered");
        let h1 = b"\\section{First}";
        let h2 = b"\\section{Second}";
        let first = store.publish_tex(&key, h1, Uuid::new_v4()).await.unwrap();
        assert_eq!(first.revision, 1);
        let copy = store.user_tex_path(Uuid::new_v4(), Uuid::new_v4());
        atomic_tex(&copy, h1).unwrap();
        let second = store.publish_tex(&key, h2, Uuid::new_v4()).await.unwrap();
        assert_eq!(second.revision, 2);
        assert_eq!(std::fs::read(&copy).unwrap(), h1);
        assert!(store.publish_tex(&key, b"", Uuid::new_v4()).await.is_err());
        assert_eq!(store.verified_tex(&key).await.unwrap().unwrap().1, h2);
        std::fs::write(store.cache_path(&key).unwrap(), b"corrupt").unwrap();
        assert!(store.verified_tex(&key).await.unwrap().is_none());
        sqlx::query("INSERT INTO global_tex_publications VALUES (?,?,?)")
            .bind(&key)
            .bind(serde_json::to_string(&second).unwrap())
            .bind(h2.as_slice())
            .execute(&store.db)
            .await
            .unwrap();
        drop(store);
        let store = FileStore::new(dir.path().into()).await.unwrap();
        assert_eq!(store.verified_tex(&key).await.unwrap().unwrap().1, h2);
    }
}
