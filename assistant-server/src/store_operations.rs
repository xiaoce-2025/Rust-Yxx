//! Durable operational metadata, separate from credentials and submission content.
use super::*;
use serde_json::{json, Value};
use sqlx::Row;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SiteFeatures {
    pub show_reprocess: bool,
}
impl Default for SiteFeatures {
    fn default() -> Self {
        Self {
            show_reprocess: true,
        }
    }
}
pub const FEATURES_PATH: &str = "admin/site_features.json";

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct OperationsQuery {
    pub kind: String,
    pub q: String,
    pub status: String,
    pub user: String,
    pub event: String,
    pub from: Option<chrono::DateTime<Utc>>,
    pub before: Option<chrono::DateTime<Utc>>,
    pub page: u32,
}

pub(super) async fn audit_detail(
    tx: &mut Transaction<'_, Sqlite>,
    id: Option<i64>,
    detail: Value,
) -> Result<(), StoreError> {
    if let Some(id) = id {
        sqlx::query("UPDATE admin_events SET detail=json_patch(detail,?) WHERE id=? AND status=0")
            .bind(detail.to_string()).bind(id).execute(&mut **tx).await?;
    }
    Ok(())
}

impl FileStore {
    pub(super) async fn init_operations(&self) -> Result<(), StoreError> {
        for statement in [
            "CREATE TABLE IF NOT EXISTS user_activity (user_id TEXT PRIMARY KEY, last_login_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS admin_events (id INTEGER PRIMARY KEY AUTOINCREMENT, actor TEXT, kind TEXT NOT NULL, target TEXT NOT NULL, status INTEGER NOT NULL, created_at TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(detail)))",
            "CREATE INDEX IF NOT EXISTS admin_events_time ON admin_events(created_at DESC, id DESC)",
            "CREATE INDEX IF NOT EXISTS admin_events_actor ON admin_events(actor, kind, id DESC)",
            "CREATE TABLE IF NOT EXISTS admin_ai_jobs (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, course TEXT NOT NULL, column_id TEXT NOT NULL, status TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, completed_at TEXT, payload TEXT NOT NULL CHECK(json_valid(payload)))",
            "CREATE INDEX IF NOT EXISTS admin_ai_jobs_time ON admin_ai_jobs(updated_at DESC, id)",
        ] { sqlx::query(statement).execute(&self.db).await?; }
        let columns = sqlx::query("PRAGMA table_info(admin_events)")
            .fetch_all(&self.db)
            .await?;
        if !columns
            .iter()
            .any(|r| r.get::<String, _>("name") == "detail")
        {
            sqlx::query("ALTER TABLE admin_events ADD COLUMN detail TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(detail))").execute(&self.db).await?;
        }
        if !columns.iter().any(|r| r.get::<String, _>("name") == "subject") {
            sqlx::query("ALTER TABLE admin_events ADD COLUMN subject TEXT")
                .execute(&self.db).await?;
        }
        sqlx::query("CREATE INDEX IF NOT EXISTS admin_events_subject ON admin_events(subject, id DESC)")
            .execute(&self.db).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS note_credit_ledger_time ON note_credit_ledger(created_at DESC,id DESC)")
            .execute(&self.db).await?;
        // Existing sessions provide a known login timestamp. Do not invent dates for older users.
        let recent_sessions: Vec<(String, DateTime<Utc>)> =
            sqlx::query_as("SELECT user_id,max(created_at) FROM sessions GROUP BY user_id")
                .fetch_all(&self.db).await?;
        for (user, created_at) in recent_sessions {
            sqlx::query("INSERT INTO user_activity VALUES (?,?) ON CONFLICT(user_id) DO UPDATE SET last_login_at=max(last_login_at,excluded.last_login_at)")
                .bind(user).bind(created_at.to_rfc3339()).execute(&self.db).await?;
        }

        // Import the latest pre-history TA records once. All future runs are archived independently.
        let rows =
            sqlx::query("SELECT name,payload FROM store_documents WHERE name >= 'sidecar/ta_ai/' AND name < 'sidecar/ta_ai0' AND payload IS NOT NULL")
                .fetch_all(&self.db)
                .await?;
        for row in rows {
            let name: String = row.get("name");
            let payload: String = row.get("payload");
            let Some(user) = name
                .strip_prefix("sidecar/ta_ai/")
                .and_then(|s| s.split('/').next())
                .and_then(|s| s.parse::<Uuid>().ok())
            else {
                continue;
            };
            let Ok(job) = serde_json::from_str::<crate::services::ta_ai::Job>(&payload) else {
                continue;
            };
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admin_ai_jobs WHERE id=?)")
                    .bind(job.id.to_string())
                    .fetch_one(&self.db)
                    .await?;
            if !exists {
                let mut tx = self.db.begin().await?;
                Self::archive_ai(&mut tx, user, &job).await?;
                tx.commit().await?;
            }
        }
        sqlx::query("UPDATE admin_ai_jobs SET status='interrupted', completed_at=?, payload=json_set(payload,'$.status','interrupted','$.message','服务重启，任务中断；已生成草稿保留') WHERE status IN ('queued','running')")
            .bind(Utc::now().to_rfc3339()).execute(&self.db).await?;
        Ok(())
    }
    pub async fn last_logins(&self) -> Result<HashMap<String, String>, StoreError> {
        Ok(
            sqlx::query("SELECT user_id,last_login_at FROM user_activity")
                .fetch_all(&self.db)
                .await?
                .into_iter()
                .map(|r| (r.get(0), r.get(1)))
                .collect(),
        )
    }
    pub(super) async fn insert_session_record(
        &self,
        session: FileUserSession,
    ) -> Result<(), StoreError> {
        let mut tx = self.begin_note_transaction().await?;
        let is_new: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM sessions WHERE id=?)")
            .bind(session.session_id.to_string()).fetch_one(&mut *tx).await?;
        write_session(&mut tx, &session).await?;

        if is_new {
            sqlx::query("INSERT INTO user_activity VALUES (?,?) ON CONFLICT(user_id) DO UPDATE SET last_login_at=max(last_login_at,excluded.last_login_at)")
                .bind(session.user_id.to_string()).bind(session.created_at.to_rfc3339()).execute(&mut *tx).await?;
            let detail = json!({"ip_address":session.ip_address.as_ref().map(|s|s.chars().take(128).collect::<String>()),"user_agent":session.user_agent.as_ref().map(|s|s.chars().take(512).collect::<String>())});
            sqlx::query("INSERT INTO admin_events(actor,kind,target,status,created_at,detail) VALUES (?,'login','session',200,?,?)")
                .bind(session.user_id.to_string()).bind(session.created_at.to_rfc3339()).bind(detail.to_string()).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    #[cfg(test)]
    async fn record_event(
        &self,
        actor: Option<Uuid>,
        kind: &str,
        target: &str,
        status: u16,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO admin_events(actor,kind,target,status,created_at) VALUES (?,?,?,?,?)",
        )
        .bind(actor.map(|u| u.to_string()))
        .bind(kind)
        .bind(target)
        .bind(status as i32)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn begin_audit(
        &self,
        actor: Option<Uuid>,
        subject: Option<Uuid>,
        kind: &str,
        target: &str,
        detail: &Value,
    ) -> Result<i64, StoreError> {
        Ok(sqlx::query("INSERT INTO admin_events(actor,subject,kind,target,status,created_at,detail) VALUES (?,?,?,?,0,?,?)")
            .bind(actor.map(|id| id.to_string()))
            .bind(subject.map(|id| id.to_string()))
            .bind(kind)
            .bind(target)
            .bind(Utc::now().to_rfc3339())
            .bind(detail.to_string())
            .execute(&self.db).await?.last_insert_rowid())
    }

    pub async fn attribute_audit(&self, id: i64, actor: Option<Uuid>, subject: Option<Uuid>) -> Result<(), StoreError> {
        sqlx::query("UPDATE admin_events SET actor=coalesce(?,actor),subject=coalesce(?,subject) WHERE id=? AND status=0")
            .bind(actor.map(|id| id.to_string())).bind(subject.map(|id| id.to_string()))
            .bind(id).execute(&self.db).await?;
        Ok(())
    }

    pub async fn describe_audit(&self, id: i64, kind: &str, detail: &Value) -> Result<(), StoreError> {
        sqlx::query("UPDATE admin_events SET kind=?,detail=json_patch(detail,?) WHERE id=? AND status=0")
            .bind(kind).bind(detail.to_string()).bind(id).execute(&self.db).await?;
        Ok(())
    }

    pub async fn finish_audit(
        &self,
        id: i64,
        status: u16,
        detail: &Value,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE admin_events SET status=?,detail=json_patch(detail,?) WHERE id=? AND status=0")
            .bind(i32::from(status))
            .bind(detail.to_string())
            .bind(id)
            .execute(&self.db).await?;
        Ok(())
    }
    async fn archive_ai(
        tx: &mut Transaction<'_, Sqlite>,
        user: Uuid,
        job: &crate::services::ta_ai::Job,
    ) -> Result<(), StoreError> {
        let now = if job.updated_at.is_empty() {
            Utc::now().to_rfc3339()
        } else {
            job.updated_at.clone()
        };
        let payload = json!({"id":job.id,"user_id":user,"course":job.course,"column":job.column,"status":job.status,"mode":job.mode,
            "total":job.total,"processed":job.processed,"drafted":job.drafted,"skipped":job.skipped,"failed":job.failures.len(),
            "include_graded":job.include_graded,"message":job.message,"updated_at":now,"model":"deepseek-flash",
            "failures":job.failures.iter().map(|f|json!({"attempt":f.attempt,"message":f.message})).collect::<Vec<_>>()});
        sqlx::query("INSERT INTO admin_ai_jobs VALUES (?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET status=excluded.status,updated_at=excluded.updated_at,completed_at=excluded.completed_at,payload=excluded.payload")
            .bind(job.id.to_string()).bind(user.to_string()).bind(&job.course).bind(&job.column).bind(&job.status).bind(&now).bind(&now)
            .bind(if job.active(){None}else{Some(now.clone())}).bind(payload.to_string()).execute(&mut **tx).await?;
        Ok(())
    }
    pub async fn save_ta_ai_job(
        &self,
        user: Uuid,
        path: &std::path::Path,
        job: &crate::services::ta_ai::Job,
    ) -> Result<(), String> {
        let mut tx = self.db.begin().await.map_err(|e| e.to_string())?;
        write_document(
            &mut tx,
            &Self::sidecar_key(path),
            &serde_json::to_string(job).map_err(|e| e.to_string())?,
        )
        .await
        .map_err(|e| e.to_string())?;
        Self::archive_ai(&mut tx, user, job)
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())
    }
    pub async fn operations_page(&self, query: &OperationsQuery) -> Result<Value, StoreError> {
        let (table, search, owner, subject, status, event, order, projection) = match query.kind.as_str() {
            "ai_jobs" => ("admin_ai_jobs", "payload", "user_id", "NULL", "status", "NULL", "updated_at DESC,id",
                "id,created_at,completed_at,json_remove(payload,'$.failures') AS payload"),
            "credits" => ("note_credit_ledger",
                "id || ' ' || coalesce(job_id,'') || ' ' || user_id || ' ' || coalesce(actor_id,'') || ' ' || coalesce(reason,'')",
                "user_id", "actor_id", "event_type", "NULL", "created_at DESC,id DESC", "*"),
            _ => ("admin_events",
                "'#' || id || ' ' || kind || ' ' || target || ' ' || coalesce(actor,'') || ' ' || coalesce(subject,'') || ' ' || detail",
                "actor", "subject", "status", "kind", "id DESC", "*"),
        };
        let filtered = |select, suffix| {
            let mut sql = sqlx::QueryBuilder::<Sqlite>::new(format!("SELECT {select} FROM {table} WHERE 1=1"));
            if let Some(id) = query.q.strip_prefix('#').and_then(|id| id.parse::<i64>().ok())
                .filter(|_| query.kind == "activity") {
                sql.push(" AND id=").push_bind(id);
            } else if !query.q.is_empty() {
                sql.push(format!(" AND instr(lower({search}), lower("))
                    .push_bind(query.q.as_str()).push("))>0");
            }
            if !query.status.is_empty() {
                sql.push(format!(" AND {status}=")).push_bind(query.status.as_str());
            }
            if !query.event.is_empty() {
                sql.push(format!(" AND {event}=")).push_bind(query.event.as_str());
            }
            if !query.user.is_empty() {
                sql.push(format!(" AND ({owner}=")).push_bind(query.user.as_str())
                    .push(format!(" OR {subject}=")).push_bind(query.user.as_str()).push(")");
            }
            if let Some(from) = query.from {
                sql.push(" AND julianday(created_at)>=julianday(").push_bind(from.to_rfc3339()).push(")");
            }
            if let Some(before) = query.before {
                sql.push(" AND julianday(created_at)<julianday(").push_bind(before.to_rfc3339()).push(")");
            }
            sql.push(suffix);
            sql
        };
        let mut tx = self.db.begin().await?;
        let total: i64 = filtered("count(*)", "").build_query_scalar().fetch_one(&mut *tx).await?;
        let page = query.page.max(1).min(((total + 49) / 50).max(1) as u32);
        let order = format!(" ORDER BY {order} LIMIT 50 OFFSET ");
        let rows = filtered(projection, order.as_str()).push_bind(i64::from(page - 1) * 50)
            .build().fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let records: Vec<Value> = rows.into_iter().map(|row| match query.kind.as_str() {
            "ai_jobs" => {
                let mut value: Value = serde_json::from_str(row.get::<&str, _>("payload")).unwrap_or(json!({}));
                value["created_at"] = json!(row.get::<String, _>("created_at"));
                value["completed_at"] = json!(row.get::<Option<String>, _>("completed_at"));
                value
            }
            "credits" => {
                let mut value = super::note_billing::ledger_entry(&row);
                value["user_id"] = json!(row.get::<String, _>("user_id"));
                value
            }
            _ => json!({
                "id": row.get::<i64, _>("id"),
                "user_id": row.get::<Option<String>, _>("actor"),
                "subject_id": row.get::<Option<String>, _>("subject"),
                "kind": row.get::<String, _>("kind"),
                "target": row.get::<String, _>("target"),
                "status": row.get::<i64, _>("status"),
                "created_at": row.get::<String, _>("created_at"),
                "detail": serde_json::from_str::<Value>(row.get::<&str, _>("detail")).unwrap_or(json!({})),
            }),
        }).collect();
        Ok(json!({"records":records,"total":total,"page":page,"page_size":50}))
    }
    pub async fn site_features(&self) -> Result<SiteFeatures, String> {
        Ok(self
            .load_sidecar(std::path::Path::new(FEATURES_PATH))
            .await?
            .unwrap_or_default())
    }
    pub async fn ai_job_record(&self, id: Uuid) -> Result<Option<Value>, StoreError> {
        let row =
            sqlx::query("SELECT payload,created_at,completed_at FROM admin_ai_jobs WHERE id=?")
                .bind(id.to_string())
                .fetch_optional(&self.db)
                .await?;
        row.map(|r| {
            let mut value: Value = serde_json::from_str(r.get::<&str, _>("payload"))?;
            value["created_at"] = json!(r.get::<String, _>("created_at"));
            value["completed_at"] = json!(r.get::<Option<String>, _>("completed_at"));
            Ok(value)
        })
        .transpose()
    }
    pub async fn save_admin_setting(
        &self,
        actor: Uuid,
        path: &std::path::Path,
        payload: &str,
    ) -> Result<(), String> {
        let after: Value = serde_json::from_str(payload).map_err(|e| e.to_string())?;
        let key = Self::sidecar_key(path);
        let mut tx = self.db.begin().await.map_err(|e| e.to_string())?;
        let before: Option<String> =
            sqlx::query_scalar("SELECT payload FROM store_documents WHERE name=?")
                .bind(&key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        let before = before.and_then(|s| serde_json::from_str::<Value>(&s).ok());
        write_document(&mut tx, &key, payload)
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query("INSERT INTO admin_events(actor,kind,target,status,created_at,detail) VALUES (?,'admin.settings',?,200,?,?)")
            .bind(actor.to_string()).bind(path.to_string_lossy().as_ref()).bind(Utc::now().to_rfc3339()).bind(json!({"before":before,"after":after}).to_string()).execute(&mut *tx).await.map_err(|e|e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(kind: &str, q: &str, status: &str, user: &str, page: u32) -> OperationsQuery {
        OperationsQuery {
            kind: kind.into(), q: q.into(), status: status.into(), user: user.into(), page,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn unfinished_request_survives_restart_and_supports_exact_lookup() {
        let tmp = tempdir::TempDir::new("audit-pending").unwrap();
        let actor = Uuid::new_v4();
        let subject = Uuid::new_v4();
        let store = FileStore::new(tmp.path().into()).await.unwrap();
        let id = store.begin_audit(Some(actor), Some(subject), "admin.change", "POST /example", &json!({}))
            .await.unwrap();
        drop(store);
        let store = FileStore::new(tmp.path().into()).await.unwrap();
        let mut filters = query("activity", &format!("#{id}"), "0", &subject.to_string(), 1);
        filters.event = "admin.change".into();
        filters.from = Some(Utc::now() - chrono::Duration::minutes(1));
        filters.before = Some(Utc::now() + chrono::Duration::minutes(1));
        let pending = store.operations_page(&filters).await.unwrap();
        assert_eq!(pending["total"], 1);
        assert_eq!(pending["records"][0]["user_id"], actor.to_string());
        store.finish_audit(id, 403, &json!({"duration_ms": 12})).await.unwrap();
        assert_eq!(store.operations_page(&filters).await.unwrap()["total"], 0);
        filters.status = "403".into();
        assert_eq!(store.operations_page(&filters).await.unwrap()["records"][0]["detail"]["duration_ms"], 12);
        filters.before = filters.from;
        assert_eq!(store.operations_page(&filters).await.unwrap()["total"], 0);
    }

    use chrono::Duration;
    #[tokio::test]
    async fn login_history_survives_session_revocation_and_settings_are_audited_atomically() {
        let tmp = tempdir::TempDir::new("operations").unwrap();
        let store = FileStore::new(tmp.path().into()).await.unwrap();
        let user = Uuid::new_v4();
        assert!(store.last_logins().await.unwrap().is_empty());
        let time = Utc::now() - Duration::days(1);
        let mut session = FileUserSession {
            session_id: Uuid::new_v4(),
            user_id: user,
            expires_at: Utc::now() + Duration::days(1),
            created_at: time,
            user_agent: Some("test browser".into()),
            ip_address: Some("127.0.0.1".into()),
        };
        store.insert_session(session.clone()).await.unwrap();
        assert_eq!(
            store.last_logins().await.unwrap()[&user.to_string()],
            time.to_rfc3339()
        );
        // Saving an existing session is not a second login.
        session.expires_at += Duration::days(1);
        store.insert_session(session.clone()).await.unwrap();
        assert_eq!(
            store
                .operations_page(&query("activity", "login", "", &user.to_string(), 1))
                .await
                .unwrap()["total"],
            1
        );
        session.session_id = Uuid::new_v4();
        session.created_at = Utc::now();
        let last = session.created_at.to_rfc3339();
        store.insert_session(session).await.unwrap();
        store.admin_revoke_sessions(user, None).await.unwrap();
        store
            .save_admin_setting(
                user,
                std::path::Path::new(FEATURES_PATH),
                r#"{"show_reprocess":false}"#,
            )
            .await
            .unwrap();
        let settings = store
            .operations_page(&query("activity", "admin.settings", "", "", 1))
            .await
            .unwrap();
        assert_eq!(
            settings["records"][0]["detail"]["after"]["show_reprocess"],
            false
        );
        let restored = FileStore::new(tmp.path().into()).await.unwrap();
        assert_eq!(
            restored.last_logins().await.unwrap()[&user.to_string()],
            last
        );
        assert!(!restored.site_features().await.unwrap().show_reprocess);
        let history = restored
            .operations_page(&query("activity", "login", "", &user.to_string(), 1))
            .await
            .unwrap();
        assert_eq!(history["total"], 2);
        assert_eq!(
            history["records"][0]["detail"]["user_agent"],
            "test browser"
        );
        for i in 0..61 {
            restored
                .record_event(Some(user), "note.read", &format!("note-{i}"), 200)
                .await
                .unwrap();
        }
        let page = restored
            .operations_page(&query("activity", "note.read", "200", &user.to_string(), 2))
            .await
            .unwrap();
        assert_eq!(page["total"], 61);
        assert_eq!(page["records"].as_array().unwrap().len(), 11);
        assert_eq!(
            restored
                .operations_page(&query("activity", "' OR 1=1 --", "", "", 1))
                .await
                .unwrap()["total"],
            0
        );
    }
    #[tokio::test]
    async fn each_ai_run_is_retained_and_restart_marks_only_active_runs_interrupted() {
        let tmp = tempdir::TempDir::new("ai-archive").unwrap();
        let store = FileStore::new(tmp.path().into()).await.unwrap();
        let user = Uuid::new_v4();
        let path = PathBuf::from("ta_ai")
            .join(user.to_string())
            .join("_c._h.json");
        let mut job:crate::services::ta_ai::Job=serde_json::from_value(json!({"id":Uuid::new_v4(),"course":"_c","column":"_h","question":"SECRET_QUESTION","rubric":"SECRET_RUBRIC","mode":"symbolic","include_graded":false,"status":"queued","message":"排队","total":1,"processed":0,"drafted":0,"skipped":0,"failures":[],"updated_at":"2026-11-14T01:00:00+00:00"})).unwrap();
        store.save_ta_ai_job(user, &path, &job).await.unwrap();
        let first = job.id;
        job.status = "completed".into();
        job.processed = 1;
        job.drafted = 1;
        job.updated_at = "2026-11-14T01:10:00+00:00".into();
        store.save_ta_ai_job(user, &path, &job).await.unwrap();
        job.id = Uuid::new_v4();
        job.status = "running".into();
        store.save_ta_ai_job(user, &path, &job).await.unwrap();
        let restored = FileStore::new(tmp.path().into()).await.unwrap();
        let records = restored
            .operations_page(&query("ai_jobs", "", "", &user.to_string(), 1))
            .await
            .unwrap();
        assert_eq!(records["total"], 2);
        let first = records["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == first.to_string())
            .unwrap();
        assert_eq!(first["status"], "completed");
        assert_eq!(first["created_at"], "2026-11-14T01:00:00+00:00");
        assert_eq!(
            restored
                .operations_page(&query("ai_jobs", "", "interrupted", "", 1))
                .await
                .unwrap()["total"],
            1
        );
        assert!(!records.to_string().contains("SECRET_"));
        // A legacy latest-job sidecar is backfilled without overwriting the preserved history.
        job.id = Uuid::new_v4();
        job.status = "cancelled".into();
        restored
            .save_sidecar(&path, &serde_json::to_string(&job).unwrap())
            .await
            .unwrap();
        let migrated = FileStore::new(tmp.path().into()).await.unwrap();
        assert_eq!(
            migrated
                .operations_page(&query("ai_jobs", "", "", "", 1))
                .await
                .unwrap()["total"],
            3
        );
    }
}
