use super::{FileStore, StoreError};
use crate::error::ApiError;
use crate::services::{
    acceleration::{AccelerationSettings, ACCELERATED_PRIORITY},
    pkuvideo::PipelineStatus,
};
use chrono::Utc;
use serde::Serialize;
use sqlx::{Sqlite, Transaction};
use std::path::Path;
use uuid::Uuid;

pub(super) async fn migrate(tx: &mut Transaction<'_, Sqlite>) -> Result<(), StoreError> {
    let migrated: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM note_billing_migrations WHERE version=3)")
            .fetch_one(&mut **tx)
            .await?;
    if migrated {
        return Ok(());
    }
    for sql in [
        "ALTER TABLE note_credit_ledger RENAME TO note_credit_ledger_v2",
        "CREATE TABLE note_credit_ledger(id TEXT PRIMARY KEY,user_id TEXT NOT NULL,job_id TEXT,event_type TEXT NOT NULL CHECK(event_type IN ('ADMIN_GRANT','JOB_CHARGE','ADMIN_ADJUST','RECHARGE','ACCELERATION_PURCHASE')),delta INTEGER NOT NULL,balance_after INTEGER NOT NULL,duration_seconds INTEGER,cost_units INTEGER,actor_id TEXT,reason TEXT,idempotency_key TEXT NOT NULL UNIQUE,created_at TEXT NOT NULL)",
        "INSERT INTO note_credit_ledger SELECT * FROM note_credit_ledger_v2",
        "DROP TABLE note_credit_ledger_v2",
        "CREATE UNIQUE INDEX idx_note_credit_one_charge_per_job ON note_credit_ledger(job_id) WHERE event_type='JOB_CHARGE'",
        "CREATE INDEX idx_note_credit_ledger_user_time ON note_credit_ledger(user_id,created_at DESC,id DESC)",
        "CREATE TABLE acceleration_ticket_accounts(user_id TEXT PRIMARY KEY, tickets INTEGER NOT NULL CHECK(typeof(tickets)='integer' AND tickets>=0))",
    ] {
        sqlx::query(sql).execute(&mut **tx).await?;
    }
    sqlx::query("INSERT INTO note_billing_migrations(version,enabled_at) VALUES(3,?)")
        .bind(Utc::now().to_rfc3339())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

const SETTINGS_KEY: &str = "acceleration_settings";

#[derive(Serialize, sqlx::FromRow)]
pub struct AccelerationWallet {
    pub balance: i64,
    pub tickets: i64,
}

const WALLET_SQL: &str = "SELECT balance, COALESCE(
    (SELECT tickets FROM acceleration_ticket_accounts WHERE user_id=a.user_id), 0
) AS tickets FROM note_credit_accounts a WHERE user_id=?";

pub(crate) fn storage_error(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(%error, "加速券存储操作失败");
    ApiError::Internal("暂时无法处理加速券，请稍后重试".into())
}

impl FileStore {
    pub async fn acceleration_settings(&self) -> Result<AccelerationSettings, ApiError> {
        Ok(self
            .load_sidecar(Path::new(SETTINGS_KEY))
            .await
            .map_err(storage_error)?
            .unwrap_or_default())
    }

    pub async fn save_acceleration_settings(
        &self,
        actor: Uuid,
        settings: AccelerationSettings,
    ) -> Result<(), ApiError> {
        let payload = serde_json::to_string(&settings).map_err(storage_error)?;
        self.save_admin_setting(actor, Path::new(SETTINGS_KEY), &payload)
            .await
            .map_err(storage_error)
    }

    pub async fn acceleration_wallet(&self, user: Uuid) -> Result<AccelerationWallet, ApiError> {
        sqlx::query_as(WALLET_SQL)
            .bind(user.to_string())
            .fetch_one(&self.db)
            .await
            .map_err(storage_error)
    }

    pub async fn buy_acceleration_ticket(
        &self,
        user: Uuid,
        request_id: Uuid,
        expected_price: i64,
    ) -> Result<AccelerationWallet, ApiError> {
        let mut tx = self.begin_note_transaction().await.map_err(storage_error)?;
        let user = user.to_string();
        let key = format!("acceleration:{user}:{request_id}");
        let previous: Option<i64> =
            sqlx::query_scalar("SELECT -delta FROM note_credit_ledger WHERE idempotency_key=?")
                .bind(&key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?;
        if previous.is_some_and(|price| price != expected_price) {
            return Err(ApiError::Conflict("购买请求与原订单不一致".into()));
        }
        if previous.is_none() {
            let raw: Option<String> =
                sqlx::query_scalar("SELECT payload FROM collections WHERE name=?")
                    .bind(Self::sidecar_key(Path::new(SETTINGS_KEY)))
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(storage_error)?;
            let settings: AccelerationSettings = raw
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(storage_error)?
                .unwrap_or_default();
            if settings.price != expected_price {
                return Err(ApiError::Conflict("售价已更新，请重新确认购买".into()));
            }
            let now = Utc::now().to_rfc3339();
            let balance: Option<i64> = sqlx::query_scalar(
                "UPDATE note_credit_accounts SET balance=balance-?,updated_at=? WHERE user_id=? AND balance>=? RETURNING balance",
            )
            .bind(settings.price).bind(&now).bind(&user).bind(settings.price)
            .fetch_optional(&mut *tx).await.map_err(storage_error)?;
            let balance = balance.ok_or_else(|| ApiError::BadRequest("希元余额不足".into()))?;
            sqlx::query(
                "INSERT INTO acceleration_ticket_accounts(user_id,tickets) VALUES(?,1)
                ON CONFLICT(user_id) DO UPDATE SET tickets=tickets+1",
            )
            .bind(&user)
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
            sqlx::query("INSERT INTO note_credit_ledger(id,user_id,event_type,delta,balance_after,reason,idempotency_key,created_at)
                VALUES(?,?,'ACCELERATION_PURCHASE',?,?,'购买加速券',?,?)")
                .bind(Uuid::new_v4().to_string()).bind(&user).bind(-settings.price).bind(balance)
                .bind(&key).bind(&now).execute(&mut *tx).await.map_err(storage_error)?;
        }
        let wallet = sqlx::query_as(WALLET_SQL)
            .bind(&user)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(wallet)
    }

    pub async fn redeem_acceleration_ticket(
        &self,
        user: Uuid,
        id: Uuid,
    ) -> Result<chrono::DateTime<Utc>, ApiError> {
        let mut jobs = self.pkuvideo_jobs.write().await;
        let job = jobs
            .get_mut(&id)
            .filter(|job| job.user_id == user)
            .ok_or(ApiError::NotFound)?;
        if job.status != PipelineStatus::Pending || job.priority <= ACCELERATED_PRIORITY {
            return Err(ApiError::Conflict("该任务当前无法使用加速券".into()));
        }

        let mut tx = self.begin_note_transaction().await.map_err(storage_error)?;
        let now = Utc::now();
        let spent = sqlx::query("UPDATE acceleration_ticket_accounts SET tickets=tickets-1 WHERE user_id=? AND tickets>0")
            .bind(user.to_string()).execute(&mut *tx).await.map_err(storage_error)?;
        if spent.rows_affected() == 0 {
            return Err(ApiError::BadRequest("加速券不足".into()));
        }
        let mut updated = job.clone();
        updated.priority = ACCELERATED_PRIORITY;
        updated.acceleration_used = true;
        updated.updated_at = now;
        sqlx::query(
            "UPDATE collections SET payload=json_set(payload,?,json(?))
            WHERE name='pkuvideo_jobs'",
        )
        .bind(format!("$.\"{id}\""))
        .bind(serde_json::to_string(&updated).map_err(storage_error)?)
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        *job = updated;
        Ok(now)
    }
}
