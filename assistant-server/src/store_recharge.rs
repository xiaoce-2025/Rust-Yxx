use super::{FileStore, StoreError};
use crate::services::recharge::{PaymentOrder, Price};
use chrono::Utc;
use sqlx::{Sqlite, Transaction};
use uuid::Uuid;

#[derive(sqlx::FromRow)]
pub(crate) struct RechargeOrder {
    pub user_id: String,
    pub app_id: String,
    pub product_id: String,
    pub quantity: i64,
    pub credits: i64,
    pub amount: i64,
    pub environment: u8,
    pub payment_id: Option<String>,
    pub wechat_order_id: Option<String>,
}

pub(super) async fn migrate(tx: &mut Transaction<'_, Sqlite>) -> Result<(), StoreError> {
    let migrated: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM note_billing_migrations WHERE version=2)")
            .fetch_one(&mut **tx)
            .await?;
    if migrated {
        return Ok(());
    }
    for sql in [
        "ALTER TABLE note_credit_ledger RENAME TO note_credit_ledger_v1",
        "CREATE TABLE note_credit_ledger(id TEXT PRIMARY KEY,user_id TEXT NOT NULL,job_id TEXT,event_type TEXT NOT NULL CHECK(event_type IN ('ADMIN_GRANT','JOB_CHARGE','ADMIN_ADJUST','RECHARGE')),delta INTEGER NOT NULL,balance_after INTEGER NOT NULL,duration_seconds INTEGER,cost_units INTEGER,actor_id TEXT,reason TEXT,idempotency_key TEXT NOT NULL UNIQUE,created_at TEXT NOT NULL)",
        "INSERT INTO note_credit_ledger SELECT * FROM note_credit_ledger_v1",
        "DROP TABLE note_credit_ledger_v1",
        "CREATE UNIQUE INDEX idx_note_credit_one_charge_per_job ON note_credit_ledger(job_id) WHERE event_type='JOB_CHARGE'",
        "CREATE INDEX idx_note_credit_ledger_user_time ON note_credit_ledger(user_id,created_at DESC,id DESC)",
        "CREATE TABLE note_recharge_orders(
            id TEXT PRIMARY KEY, user_id TEXT NOT NULL, app_id TEXT NOT NULL, product_id TEXT NOT NULL,
            quantity INTEGER NOT NULL CHECK(quantity > 0), credits INTEGER NOT NULL CHECK(credits > 0),
            amount INTEGER NOT NULL CHECK(amount > 0), environment INTEGER NOT NULL CHECK(environment IN (0,1)),
            payment_id TEXT UNIQUE, wechat_order_id TEXT UNIQUE, created_at TEXT NOT NULL, paid_at TEXT
        )",
    ] {
        sqlx::query(sql).execute(&mut **tx).await?;
    }
    sqlx::query("INSERT INTO note_billing_migrations(version,enabled_at) VALUES(2,?)")
        .bind(Utc::now().to_rfc3339())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

impl FileStore {
    pub async fn reserve_recharge(
        &self,
        id: Uuid,
        user: Uuid,
        app: &str,
        env: u8,
        price: &Price,
    ) -> Result<Option<RechargeOrder>, StoreError> {
        let mut tx = self.begin_note_transaction().await?;
        sqlx::query("INSERT INTO note_recharge_orders(id,user_id,app_id,product_id,quantity,credits,amount,environment,created_at)
            VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO NOTHING")
            .bind(id.to_string()).bind(user.to_string()).bind(app).bind(price.product)
            .bind(price.quantity).bind(price.credits).bind(price.cents).bind(env)
            .bind(Utc::now().to_rfc3339()).execute(&mut *tx).await?;
        let order: RechargeOrder = sqlx::query_as("SELECT * FROM note_recharge_orders WHERE id=?")
            .bind(id.to_string())
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((order.user_id == user.to_string()
            && order.app_id == app
            && order.environment == env
            && order.product_id == price.product
            && order.quantity == price.quantity
            && order.credits == price.credits
            && order.amount == price.cents)
            .then_some(order))
    }

    pub async fn attach_recharge_payment(
        &self,
        id: Uuid,
        payment_id: &str,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE note_recharge_orders SET payment_id=?
            WHERE id=? AND (payment_id IS NULL OR payment_id=?)",
        )
        .bind(payment_id)
        .bind(id.to_string())
        .bind(payment_id)
        .execute(&self.db)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn recharge_by_payment(
        &self,
        user: Uuid,
        payment_id: &str,
    ) -> Result<Option<RechargeOrder>, StoreError> {
        Ok(
            sqlx::query_as("SELECT * FROM note_recharge_orders WHERE user_id=? AND payment_id=?")
                .bind(user.to_string())
                .bind(payment_id)
                .fetch_optional(&self.db)
                .await?,
        )
    }

    pub async fn settle_recharge(
        &self,
        app: &str,
        payment: &PaymentOrder,
    ) -> Result<bool, StoreError> {
        let mut tx = self.begin_note_transaction().await?;
        let id = payment.app_order_id.to_string();
        let order: Option<RechargeOrder> =
            sqlx::query_as("SELECT * FROM note_recharge_orders WHERE id=?")
                .bind(&id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(order) = order else {
            return Ok(false);
        };
        if order.app_id != app
            || order.product_id != payment.product_id
            || order.quantity != payment.quantity
            || order.amount != payment.amount
            || Some(order.environment) != payment.environment
            || order
                .payment_id
                .as_ref()
                .is_some_and(|known| known != &payment.id)
        {
            return Ok(false);
        }
        let Some(wechat_id) = payment.wechat_order_id.as_deref() else {
            return Ok(false);
        };
        if let Some(known) = order.wechat_order_id {
            return Ok(known == wechat_id);
        }

        let now = Utc::now().to_rfc3339();
        let balance: i64 = sqlx::query_scalar(
            "UPDATE note_credit_accounts SET balance=balance+?,updated_at=?
            WHERE user_id=? RETURNING balance",
        )
        .bind(order.credits)
        .bind(&now)
        .bind(&order.user_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO note_credit_ledger(id,user_id,event_type,delta,balance_after,reason,idempotency_key,created_at)
            VALUES(?,?,'RECHARGE',?,?,?,?,?)")
            .bind(Uuid::new_v4().to_string()).bind(&order.user_id).bind(order.credits).bind(balance)
            .bind(format!("希元充值 · 订单 {}", payment.id)).bind(format!("recharge:{id}")).bind(&now)
            .execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE note_recharge_orders SET payment_id=?,wechat_order_id=?,paid_at=? WHERE id=?",
        )
        .bind(&payment.id)
        .bind(wechat_id)
        .bind(&now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }
}
