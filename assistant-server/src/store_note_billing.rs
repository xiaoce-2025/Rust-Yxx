//! Authoritative note credits. All monetary mutations use SQLite write transactions.
use super::{FileStore, FileUser, StoreError};
use crate::services::pkuvideo::{PipelineStatus, PkuVideoJob};
use chrono::Utc;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

pub(super) fn ledger_entry(row: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": row.get::<String, _>("id"),
        "job_id": row.get::<Option<String>, _>("job_id"),
        "event_type": row.get::<String, _>("event_type"),
        "delta": row.get::<i64, _>("delta"),
        "balance_after": row.get::<i64, _>("balance_after"),
        "duration_seconds": row.get::<Option<i64>, _>("duration_seconds"),
        "cost_units": row.get::<Option<i64>, _>("cost_units"),
        "reason": row.get::<Option<String>, _>("reason"),
        "actor_id": row.get::<Option<String>, _>("actor_id"),
        "created_at": row.get::<String, _>("created_at"),
    })
}

pub const INSUFFICIENT: &str = "INSUFFICIENT_NOTE_CREDITS";
pub const INSUFFICIENT_MESSAGE: &str = "希元余额不足，请联系管理员发放希元";

pub fn cost_units(seconds: i64) -> Result<i64, StoreError> {
    if seconds <= 0 {
        return Err("InvalidDuration".into());
    }
    const SECONDS_PER_HOUR: i64 = 60 * 60;
    const GRACE_SECONDS: i64 = 5 * 60;
    let whole_hours = seconds / SECONDS_PER_HOUR;
    let extra_hour = i64::from(seconds % SECONDS_PER_HOUR > GRACE_SECONDS);
    Ok((whole_hours + extra_hour).max(1))
}

/// Parse ffprobe's decimal seconds without rounding through floating point.
pub fn duration_seconds(raw: &str) -> Result<i64, StoreError> {
    let raw = raw.trim();
    let (whole, fraction) = raw.split_once('.').unwrap_or((raw, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return Err("InvalidDuration".into());
    }
    let value = whole
        .parse::<i64>()?
        .checked_add(i64::from(fraction.bytes().any(|b| b != b'0')))
        .ok_or("Overflow")?;
    cost_units(value)?;
    Ok(value)
}

impl FileStore {
    pub(super) async fn begin_note_transaction(
        &self,
    ) -> Result<sqlx::Transaction<'_, sqlx::Sqlite>, StoreError> {
        for attempt in 0..3 {
            match self.db.begin_with("BEGIN IMMEDIATE").await {
                Ok(tx) => return Ok(tx),
                Err(error)
                    if attempt < 2
                        && error.as_database_error().is_some_and(|e| {
                            matches!(e.code().as_deref(), Some("5" | "6" | "261" | "517"))
                        }) =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(40 * (attempt + 1))).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!()
    }
    pub(super) async fn init_note_billing(&self) -> Result<(), StoreError> {
        let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='note_billing_migrations'")
            .fetch_one(&self.db).await?;
        if exists == 0 {
            // SQLite's own snapshot includes committed WAL pages. Never copy a live DB file.
            let backup = self.data_dir.join(format!(
                "assistant.pre-note-billing-{}.sqlite3",
                Uuid::new_v4()
            ));
            sqlx::query("VACUUM INTO ?")
                .bind(backup.to_string_lossy().as_ref())
                .execute(&self.db)
                .await?;
        }
        let mut tx = self.begin_note_transaction().await?;
        for sql in [
            "CREATE TABLE IF NOT EXISTS note_billing_migrations(version INTEGER PRIMARY KEY, enabled_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS note_credit_accounts(user_id TEXT PRIMARY KEY,balance INTEGER NOT NULL DEFAULT 0 CHECK(typeof(balance)='integer'),updated_at TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS note_credit_ledger(id TEXT PRIMARY KEY,user_id TEXT NOT NULL,job_id TEXT,event_type TEXT NOT NULL CHECK(event_type IN ('ADMIN_GRANT','JOB_CHARGE','ADMIN_ADJUST')),delta INTEGER NOT NULL,balance_after INTEGER NOT NULL,duration_seconds INTEGER,cost_units INTEGER,actor_id TEXT,reason TEXT,idempotency_key TEXT NOT NULL UNIQUE,created_at TEXT NOT NULL)",
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_note_credit_one_charge_per_job ON note_credit_ledger(job_id) WHERE event_type='JOB_CHARGE'",
            "CREATE INDEX IF NOT EXISTS idx_note_credit_ledger_user_time ON note_credit_ledger(user_id,created_at DESC,id DESC)",
            "CREATE TABLE IF NOT EXISTS note_job_billing(job_id TEXT PRIMARY KEY,user_id TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('QUEUED','START_ALLOWED','REJECTED_INSUFFICIENT','BILLING_PENDING','CHARGED','FAILED_NO_CHARGE')),balance_at_start INTEGER,checked_at TEXT,duration_seconds INTEGER,cost_units INTEGER,charged_at TEXT,failure_code TEXT,updated_at TEXT NOT NULL)",
        ] { sqlx::query(sql).execute(&mut *tx).await?; }
        let migrated: i64 =
            sqlx::query_scalar("SELECT count(*) FROM note_billing_migrations WHERE version=1")
                .fetch_one(&mut *tx)
                .await?;
        if migrated == 0 {
            let now = Utc::now().to_rfc3339();
            for user in self.users.read().await.values() {
                sqlx::query("INSERT INTO note_credit_accounts(user_id,balance,updated_at) VALUES(?,?,?) ON CONFLICT(user_id) DO NOTHING")
                    .bind(user.id.to_string()).bind(user.processing_quota).bind(&now).execute(&mut *tx).await?;
            }
            sqlx::query("INSERT INTO note_billing_migrations(version,enabled_at) VALUES(1,?)")
                .bind(now)
                .execute(&mut *tx)
                .await?;
        }
        super::recharge::migrate(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn note_balance(&self, user: Uuid) -> Result<i64, StoreError> {
        Ok(
            sqlx::query_scalar("SELECT balance FROM note_credit_accounts WHERE user_id=?")
                .bind(user.to_string())
                .fetch_one(&self.db)
                .await?,
        )
    }
    pub(super) async fn project_note_balance(&self, mut user: FileUser) -> Option<FileUser> {
        match self.note_balance(user.id).await {
            Ok(balance) => {
                user.processing_quota = balance;
                Some(user)
            }
            Err(error) => {
                tracing::error!(user_id=%user.id,%error,"读取希元余额失败");
                None
            }
        }
    }
    pub async fn queue_note_billing(&self, job: Uuid, user: Uuid) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO note_job_billing(job_id,user_id,state,updated_at) VALUES(?,?,'QUEUED',?) ON CONFLICT(job_id) DO NOTHING")
            .bind(job.to_string()).bind(user.to_string()).bind(Utc::now().to_rfc3339()).execute(&self.db).await?;
        Ok(())
    }
    pub async fn check_note_start(&self, job: Uuid, user: Uuid) -> Result<bool, StoreError> {
        let mut tx = self.begin_note_transaction().await?;
        let previous = sqlx::query("SELECT user_id,state FROM note_job_billing WHERE job_id=?")
            .bind(job.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(row) = previous {
            if row.get::<String, _>("user_id") != user.to_string() {
                return Err("BillingOwnerMismatch".into());
            }
            match row.get::<String, _>("state").as_str() {
                "START_ALLOWED" | "BILLING_PENDING" => {
                    tx.commit().await?;
                    return Ok(true);
                }
                "REJECTED_INSUFFICIENT" | "FAILED_NO_CHARGE" | "CHARGED" => {
                    tx.commit().await?;
                    return Ok(false);
                }
                _ => {}
            }
        }
        let balance: i64 =
            sqlx::query_scalar("SELECT balance FROM note_credit_accounts WHERE user_id=?")
                .bind(user.to_string())
                .fetch_one(&mut *tx)
                .await?;
        let now = Utc::now().to_rfc3339();
        sqlx::query("INSERT INTO note_job_billing(job_id,user_id,state,balance_at_start,checked_at,failure_code,updated_at) VALUES(?,?,?,?,?,?,?) ON CONFLICT(job_id) DO UPDATE SET state=excluded.state,balance_at_start=excluded.balance_at_start,checked_at=excluded.checked_at,failure_code=excluded.failure_code,updated_at=excluded.updated_at")
            .bind(job.to_string()).bind(user.to_string()).bind(if balance>0 {"START_ALLOWED"} else {"REJECTED_INSUFFICIENT"})
            .bind(balance).bind(&now).bind(if balance>0 {None} else {Some(INSUFFICIENT)}).bind(&now).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(balance > 0)
    }

    /// Idempotent, incremental admin grant; never replaces the balance or erases debt.
    pub async fn grant_note_credits(
        &self,
        user: Uuid,
        actor: Uuid,
        amount: i64,
        reason: &str,
        key: &str,
        audit_id: Option<i64>,
    ) -> Result<Value, StoreError> {
        if amount <= 0
            || reason.trim().is_empty()
            || reason.len() > 1000
            || key.is_empty()
            || key.len() > 128
        {
            return Err("InvalidGrant".into());
        }
        let key = format!("grant:{actor}:{key}");
        let mut tx = self.begin_note_transaction().await?;
        if let Some(row) = sqlx::query("SELECT id,user_id,delta,balance_after,reason FROM note_credit_ledger WHERE idempotency_key=?").bind(&key).fetch_optional(&mut *tx).await? {
            if row.get::<String,_>("user_id") != user.to_string() || row.get::<i64,_>("delta") != amount || row.get::<String,_>("reason") != reason.trim() { return Err("IdempotencyConflict".into()); }
            let after: i64=row.get("balance_after");
            super::operations::audit_detail(&mut tx, audit_id, json!({"ledger_id":row.get::<String,_>("id"),"idempotent_replay":true})).await?;
            tx.commit().await?;
            return Ok(json!({"balance_before":after-amount,"balance_after":after,"granted":amount}));
        }
        let before: i64 =
            sqlx::query_scalar("SELECT balance FROM note_credit_accounts WHERE user_id=?")
                .bind(user.to_string())
                .fetch_one(&mut *tx)
                .await?;
        let after = before.checked_add(amount).ok_or("Overflow")?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE note_credit_accounts SET balance=balance+?,updated_at=? WHERE user_id=?",
        )
        .bind(amount)
        .bind(&now)
        .bind(user.to_string())
        .execute(&mut *tx)
        .await?;
        let ledger_id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO note_credit_ledger(id,user_id,event_type,delta,balance_after,actor_id,reason,idempotency_key,created_at) VALUES(?,?,'ADMIN_GRANT',?,?,?,?,?,?)")
            .bind(&ledger_id).bind(user.to_string()).bind(amount).bind(after).bind(actor.to_string()).bind(reason.trim()).bind(key).bind(now).execute(&mut *tx).await?;
        super::operations::audit_detail(&mut tx, audit_id, json!({"ledger_id":ledger_id,"idempotent_replay":false})).await?;
        tx.commit().await?;
        Ok(json!({"balance_before":before,"balance_after":after,"granted":amount}))
    }

    /// Charge and final job state are committed together. The store mirror lock has the
    /// same order as ordinary job updates; callers must release AppState's mirror first.
    pub async fn charge_note_job(
        &self,
        mut job: PkuVideoJob,
        seconds: i64,
    ) -> Result<PkuVideoJob, StoreError> {
        let units = cost_units(seconds)?;
        let mut jobs = self.pkuvideo_jobs.write().await;
        let mut tx = self.begin_note_transaction().await?;
        if let Some(row) = sqlx::query(
            "SELECT user_id FROM note_credit_ledger WHERE job_id=? AND event_type='JOB_CHARGE'",
        )
        .bind(job.id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        {
            if row.get::<String, _>("user_id") != job.user_id.to_string() {
                return Err("BillingOwnerMismatch".into());
            }
            let payload: String =
                sqlx::query_scalar("SELECT payload FROM collections WHERE name='pkuvideo_jobs'")
                    .fetch_one(&mut *tx)
                    .await?;
            let persisted: std::collections::HashMap<Uuid, PkuVideoJob> =
                serde_json::from_str(&payload)?;
            let result = persisted.get(&job.id).cloned().ok_or("任务不存在")?;
            tx.commit().await?;
            jobs.insert(result.id, result.clone());
            return Ok(result);
        }
        let row = sqlx::query("SELECT user_id,state FROM note_job_billing WHERE job_id=?")
            .bind(job.id.to_string())
            .fetch_one(&mut *tx)
            .await?;
        if row.get::<String, _>("user_id") != job.user_id.to_string()
            || !matches!(
                row.get::<String, _>("state").as_str(),
                "START_ALLOWED" | "BILLING_PENDING"
            )
        {
            return Err("BillingStartNotAllowed".into());
        }
        let before: i64 =
            sqlx::query_scalar("SELECT balance FROM note_credit_accounts WHERE user_id=?")
                .bind(job.user_id.to_string())
                .fetch_one(&mut *tx)
                .await?;
        let after = before.checked_sub(units).ok_or("Overflow")?;
        let now = Utc::now();
        sqlx::query(
            "UPDATE note_credit_accounts SET balance=balance-?,updated_at=? WHERE user_id=?",
        )
        .bind(units)
        .bind(now.to_rfc3339())
        .bind(job.user_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO note_credit_ledger(id,user_id,job_id,event_type,delta,balance_after,duration_seconds,cost_units,idempotency_key,created_at) VALUES(?,?,?,'JOB_CHARGE',?,?,?,?,?,?)")
            .bind(Uuid::new_v4().to_string()).bind(job.user_id.to_string()).bind(job.id.to_string()).bind(-units).bind(after).bind(seconds).bind(units).bind(format!("job:{}",job.id)).bind(now.to_rfc3339()).execute(&mut *tx).await?;
        sqlx::query("UPDATE note_job_billing SET state='CHARGED',duration_seconds=?,cost_units=?,charged_at=?,failure_code=NULL,updated_at=? WHERE job_id=?")
            .bind(seconds).bind(units).bind(now.to_rfc3339()).bind(now.to_rfc3339()).bind(job.id.to_string()).execute(&mut *tx).await?;
        job.status = PipelineStatus::Succeeded;
        job.billing_state = Some("CHARGED".into());
        job.billing_duration_seconds = Some(seconds);
        job.charged_units = units;
        job.failure_code = None;
        job.error = None;
        job.postprocessing = false;
        job.updated_at = now;
        job.finished_at = Some(now);
        let path = format!("$.\"{}\"", job.id);
        sqlx::query(
            "UPDATE collections SET payload=json_set(payload,?,json(?)) WHERE name='pkuvideo_jobs'",
        )
        .bind(path)
        .bind(serde_json::to_string(&job)?)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        jobs.insert(job.id, job.clone());
        Ok(job)
    }

    pub async fn note_billing_state(&self, job: Uuid) -> Result<Option<String>, StoreError> {
        Ok(
            sqlx::query_scalar("SELECT state FROM note_job_billing WHERE job_id=?")
                .bind(job.to_string())
                .fetch_optional(&self.db)
                .await?,
        )
    }
    pub async fn set_note_billing_state(
        &self,
        job: Uuid,
        state: &str,
        code: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE note_job_billing SET state=?,failure_code=?,updated_at=? WHERE job_id=? AND state NOT IN ('CHARGED','REJECTED_INSUFFICIENT')")
            .bind(state).bind(code).bind(Utc::now().to_rfc3339()).bind(job.to_string()).execute(&self.db).await?;
        Ok(())
    }
    pub async fn note_ledger(
        &self,
        user: Uuid,
        limit: i64,
        cursor: Option<&str>,
    ) -> Result<Value, StoreError> {
        let rows=sqlx::query("SELECT * FROM note_credit_ledger WHERE user_id=? AND (? IS NULL OR (created_at || '/' || id) < ?) ORDER BY created_at DESC,id DESC LIMIT ?")
            .bind(user.to_string()).bind(cursor).bind(cursor).bind(limit.clamp(1,100)).fetch_all(&self.db).await?;
        let entries: Vec<Value> = rows.iter().map(ledger_entry).collect();
        let next = rows.last().map(|r| {
            format!(
                "{}/{}",
                r.get::<String, _>("created_at"),
                r.get::<String, _>("id")
            )
        });
        Ok(json!({"entries":entries,"next_cursor":next}))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    async fn fixture(balance: i64) -> (tempdir::TempDir, Arc<FileStore>, Uuid) {
        let temp = tempdir::TempDir::new("note-billing").unwrap();
        let store = Arc::new(FileStore::new(temp.path().into()).await.unwrap());
        let id = Uuid::new_v4();
        store
            .insert_user(FileUser {
                id,
                username: id.to_string(),
                email: format!("{id}@example.invalid"),
                password_hash: String::new(),
                is_active: true,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                storage_quota: 3,
                processing_quota: balance,
                processing_priority: 5,
                iaaa_encrypted_password: None,
                iaaa_public_key: None,
            })
            .await
            .unwrap();
        (temp, store, id)
    }
    async fn job(store: &FileStore, user: Uuid) -> PkuVideoJob {
        let job = PkuVideoJob::new(
            Uuid::new_v4(),
            user,
            "https://example.invalid/video".into(),
            "unused".into(),
        );
        store.insert_pkuvideo_job(job.clone()).await.unwrap();
        store.queue_note_billing(job.id, user).await.unwrap();
        job
    }
    #[test]
    fn durations_and_overflow_boundaries() {
        for (seconds, units) in [
            (1, 1),
            (180, 1),
            (1799, 1),
            (3599, 1),
            (3600, 1),
            (3601, 1),
            (3900, 1),
            (3901, 2),
            (7200, 2),
            (7500, 2),
            (7501, 3),
            (9000, 3),
            (i64::MAX, i64::MAX / 3600 + 1),
        ] {
            assert_eq!(cost_units(seconds).unwrap(), units);
        }
        for value in [0, -1, i64::MIN] {
            assert!(cost_units(value).is_err());
        }
        for (raw, seconds) in [
            ("3600.1", 3601),
            ("3600.000", 3600),
            ("7500.0", 7500),
            ("7500.001", 7501),
            ("0.01", 1),
            ("59.9", 60),
        ] {
            assert_eq!(duration_seconds(raw).unwrap(), seconds);
        }
        for raw in [
            "0",
            "-1",
            "NaN",
            "inf",
            "",
            "3600 seconds",
            "9223372036854775807.1",
        ] {
            assert!(duration_seconds(raw).is_err());
        }
    }
    #[tokio::test]
    async fn concurrent_starts_negative_balance_idempotency_and_grant() {
        let (_temp, store, user) = fixture(1).await;
        let a = job(&store, user).await;
        let b = job(&store, user).await;
        assert_eq!(store.note_balance(user).await.unwrap(), 1); // queuing never reserves
        let (ar, br) = tokio::join!(
            store.check_note_start(a.id, user),
            store.check_note_start(b.id, user)
        );
        assert!(ar.unwrap() && br.unwrap());
        let (ar, br) = tokio::join!(
            store.charge_note_job(a.clone(), 7200),
            store.charge_note_job(b.clone(), 10800)
        );
        assert_eq!(ar.unwrap().charged_units, 2);
        assert_eq!(br.unwrap().charged_units, 3);
        assert_eq!(store.note_balance(user).await.unwrap(), -4);
        let mut stale_failure = a.clone();
        stale_failure.status = PipelineStatus::Failed;
        store.update_pkuvideo_job(stale_failure).await.unwrap();
        assert_eq!(
            store.get_pkuvideo_job(a.id).await.unwrap().status,
            PipelineStatus::Succeeded
        );
        for _ in 0..3 {
            assert_eq!(
                store
                    .charge_note_job(a.clone(), 7200)
                    .await
                    .unwrap()
                    .charged_units,
                2
            );
        }
        assert_eq!(
            store.note_ledger(user, 30, None).await.unwrap()["entries"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let c = job(&store, user).await;
        assert!(!store.check_note_start(c.id, user).await.unwrap());
        assert!(store.charge_note_job(c, 60).await.is_err());
        let actor = Uuid::new_v4();
        let g = store
            .grant_note_credits(user, actor, 10, "添加额度", "same", None)
            .await
            .unwrap();
        assert_eq!(g["balance_before"], -4);
        assert_eq!(g["balance_after"], 6);
        assert_eq!(
            store
                .grant_note_credits(user, actor, 10, "添加额度", "same", None)
                .await
                .unwrap(),
            g
        );
        assert!(store
            .grant_note_credits(user, actor, 11, "添加额度", "same", None)
            .await
            .is_err());
        assert_eq!(store.note_balance(user).await.unwrap(), 6);
    }
    #[tokio::test]
    async fn start_after_charge_rejects_and_failure_is_free() {
        let (_temp, store, user) = fixture(1).await;
        let a = job(&store, user).await;
        let b = job(&store, user).await;
        let failed = job(&store, user).await;
        assert!(store.check_note_start(a.id, user).await.unwrap());
        assert!(store.check_note_start(failed.id, user).await.unwrap());
        store
            .set_note_billing_state(failed.id, "FAILED_NO_CHARGE", Some("AI_FAILED"))
            .await
            .unwrap();
        assert!(store.charge_note_job(failed, 3600).await.is_err());
        store.charge_note_job(a, 7200).await.unwrap();
        assert!(!store.check_note_start(b.id, user).await.unwrap());
        assert_eq!(store.note_balance(user).await.unwrap(), -1);
        assert_eq!(store.get_user(user).await.unwrap().processing_quota, -1);
        store.admin_update_user(user, true, 7, 2, None).await.unwrap();
        assert_eq!(store.get_user(user).await.unwrap().processing_quota, -1);
    }
    #[tokio::test]
    async fn grant_racing_charge_and_transaction_rollback() {
        let (_temp, store, user) = fixture(1).await;
        let a = job(&store, user).await;
        assert!(store.check_note_start(a.id, user).await.unwrap());
        let (charged, grant) = tokio::join!(
            store.charge_note_job(a.clone(), 7200),
            store.grant_note_credits(user, Uuid::new_v4(), 10, "manual", "race", None)
        );
        charged.unwrap();
        grant.unwrap();
        assert_eq!(store.note_balance(user).await.unwrap(), 9);
        let b = job(&store, user).await;
        assert!(store.check_note_start(b.id, user).await.unwrap());
        sqlx::query("CREATE TRIGGER fail_charge BEFORE INSERT ON note_credit_ledger WHEN NEW.event_type='JOB_CHARGE' BEGIN SELECT RAISE(ABORT,'injected failure'); END").execute(&store.db).await.unwrap();
        assert!(store.charge_note_job(b.clone(), 3600).await.is_err());
        assert_eq!(store.note_balance(user).await.unwrap(), 9);
        assert_eq!(
            store.get_pkuvideo_job(b.id).await.unwrap().status,
            PipelineStatus::Pending
        );
        sqlx::query("DROP TRIGGER fail_charge")
            .execute(&store.db)
            .await
            .unwrap();
        store
            .set_note_billing_state(b.id, "BILLING_PENDING", Some("BILLING_COMMIT_FAILED"))
            .await
            .unwrap();
        store.charge_note_job(b, 3600).await.unwrap();
        assert_eq!(store.note_balance(user).await.unwrap(), 8);
    }
    #[tokio::test]
    async fn invalid_duration_owner_and_account_overflow_do_not_mutate() {
        let (_temp, store, user) = fixture(i64::MAX).await;
        let a = job(&store, user).await;
        assert!(store.check_note_start(a.id, user).await.unwrap());
        assert!(store
            .grant_note_credits(user, Uuid::new_v4(), 1, "overflow", "overflow", None)
            .await
            .is_err());
        assert!(store.charge_note_job(a.clone(), 0).await.is_err());
        let mut wrong = a.clone();
        wrong.user_id = Uuid::new_v4();
        assert!(store.charge_note_job(wrong, 3600).await.is_err());
        assert_eq!(store.note_balance(user).await.unwrap(), i64::MAX);
        sqlx::query("UPDATE note_credit_accounts SET balance=? WHERE user_id=?")
            .bind(i64::MIN)
            .bind(user.to_string())
            .execute(&store.db)
            .await
            .unwrap();
        assert!(store.charge_note_job(a, 3600).await.is_err());
        assert_eq!(store.note_balance(user).await.unwrap(), i64::MIN);
    }
    #[tokio::test]
    async fn legacy_migration_preserves_debt_and_does_not_reinject() {
        let temp = tempdir::TempDir::new("legacy-billing").unwrap();
        let user = Uuid::new_v4();
        let legacy = FileUser {
            id: user,
            username: "old".into(),
            email: "old@example.invalid".into(),
            password_hash: String::new(),
            is_active: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            storage_quota: 3,
            processing_quota: -3,
            processing_priority: 5,
            iaaa_encrypted_password: None,
            iaaa_public_key: None,
        };
        std::fs::write(
            temp.path().join("users.json"),
            serde_json::to_vec(&std::collections::HashMap::from([(user, legacy)])).unwrap(),
        )
        .unwrap();
        let store = FileStore::new(temp.path().into()).await.unwrap();
        assert_eq!(store.note_balance(user).await.unwrap(), -3);
        store
            .grant_note_credits(user, user, 10, "grant", "migration", None)
            .await
            .unwrap();
        store.db.close().await;
        drop(store);
        let store = FileStore::new(temp.path().into()).await.unwrap();
        assert_eq!(store.note_balance(user).await.unwrap(), 7);
        assert_eq!(store.get_user(user).await.unwrap().processing_quota, 7);
        assert!(std::fs::read_dir(temp.path()).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("assistant.pre-note-billing-")));
    }
    #[tokio::test]
    async fn crash_reopen_reconciles_job_and_charge_once() {
        let (temp, store, user) = fixture(1).await;
        let a = job(&store, user).await;
        assert!(store.check_note_start(a.id, user).await.unwrap());
        store
            .set_note_billing_state(a.id, "BILLING_PENDING", Some("BILLING_COMMIT_FAILED"))
            .await
            .unwrap();
        store.db.close().await;
        drop(store);
        let store = FileStore::new(temp.path().into()).await.unwrap();
        let charged = store.charge_note_job(a.clone(), 3901).await.unwrap();
        assert_eq!(charged.status, PipelineStatus::Succeeded);
        store.db.close().await;
        drop(store);
        let store = FileStore::new(temp.path().into()).await.unwrap();
        assert_eq!(
            store.charge_note_job(a, 3901).await.unwrap().charged_units,
            2
        );
        assert_eq!(store.note_balance(user).await.unwrap(), -1);
        assert_eq!(
            store
                .get_all_pkuvideo_jobs()
                .await
                .values()
                .next()
                .unwrap()
                .status,
            PipelineStatus::Succeeded
        );
    }
}
