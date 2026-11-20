use super::*;
use serde_json::{json, Value};

const USERS: &str = "SELECT u.id,u.username,u.email,u.active,
    u.id IN (SELECT value FROM json_each(?1)) AS administrator,
    b.balance, a.last_login_at, u.payload,
    json_extract(u.payload,'$.created_at') AS created_at,
    json_extract(u.payload,'$.storage_quota') AS storage_quota,
    json_extract(u.payload,'$.processing_priority') AS processing_priority,
    (SELECT count(*) FROM sessions s WHERE s.user_id=u.id AND s.expires_at>?2) AS sessions
    FROM users u JOIN note_credit_accounts b ON b.user_id=u.id
    LEFT JOIN user_activity a ON a.user_id=u.id";

const USER_JSON: &str = "json_object('id',id,'username',username,'email',email,
    'is_active',json(CASE active WHEN 1 THEN 'true' ELSE 'false' END),
    'is_admin',json(CASE administrator WHEN 1 THEN 'true' ELSE 'false' END),
    'created_at',created_at,'updated_at',json_extract(payload,'$.updated_at'),
    'storage_quota',storage_quota,'processing_quota',balance,'processing_priority',processing_priority,
    'last_login_at',last_login_at,'sessions',sessions)";

const JOB_JSON: &str = "json_object('id',id,'user_id',owner,'username',username,'kind',kind,
    'status',status,'created_at',created_at,'updated_at',updated_at,
    'course_name',json_extract(payload,'$.course_name'),'course_date',json_extract(payload,'$.course_date'),
    'regenerate',json_extract(payload,'$.regenerate'),'result_source',json_extract(payload,'$.result_source'),
    'url_key',json_extract(payload,'$.url_key'),'billing_state',json_extract(payload,'$.billing_state'),
    'charged_units',json_extract(payload,'$.charged_units'),'billing_duration_seconds',json_extract(payload,'$.billing_duration_seconds'),
    'failure_code',json_extract(payload,'$.failure_code'))";

const JOBS: &str = "SELECT j.id,j.owner,j.payload,u.username,
    json_extract(j.payload,'$.status') AS status,
    json_extract(j.payload,'$.created_at') AS created_at,
    json_extract(j.payload,'$.updated_at') AS updated_at";

#[derive(Deserialize)]
pub(crate) struct AdminList {
    pub model: String,
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub desc: bool,
    #[serde(default = "first_page")]
    pub page: i64,
    pub id: Option<String>,
}
fn first_page() -> i64 {
    1
}

impl FileStore {
    pub(crate) async fn admin_list(
        &self,
        input: AdminList,
        administrators: &[Uuid],
    ) -> Result<Value, StoreError> {
        let model = input.model.as_str();
        let (source, projection, filter, sort) = match model {
            "users" => (
                USERS.into(),
                USER_JSON,
                "(?4='' OR instr(lower(username||' '||email||' '||id),?4)>0)
                AND (?5 NOT IN ('active','inactive') OR active=(?5='active'))
                AND (?6 NOT IN ('admin','member') OR administrator=(?6='admin'))",
                match input.sort.as_str() {
                    "username" => "username COLLATE NOCASE",
                    "email" => "email COLLATE NOCASE",
                    "storage_quota" => "storage_quota",
                    "processing_quota" => "balance",
                    "processing_priority" => "processing_priority",
                    "last_login_at" => "last_login_at",
                    _ => "created_at",
                },
            ),
            "whitelist" => (
                "SELECT email AS id,email FROM registration_whitelist".into(),
                "json_object('id',id,'email',email)",
                "(?4='' OR instr(lower(email),?4)>0)",
                "email COLLATE NOCASE",
            ),
            "notes" | "replay" | "translation" => {
                let (table, kind) = match model {
                    "notes" => ("pkuvideo_jobs", "'notes'"),
                    "replay" => ("pkuvideo_fw_jobs", "'replay'"),
                    _ => ("translation_jobs", "'translation'"),
                };
                (
                    format!(
                        "{JOBS},{kind} AS kind FROM {table} j LEFT JOIN users u ON u.id=j.owner"
                    ),
                    JOB_JSON,
                    "(?4='' OR instr(lower(id||' '||coalesce(username,owner)||' '||
                        coalesce(json_extract(payload,'$.course_name'),'')||' '||
                        coalesce(json_extract(payload,'$.course_date'),'')||' '||
                        coalesce(json_extract(payload,'$.result_source'),'')||' '||
                        coalesce(json_extract(payload,'$.url_key'),'')),?4)>0)
                    AND (?5 IN ('','all') OR status=?5 OR (?5='pending' AND status='queued'))",
                    match input.sort.as_str() {
                        "id" => "id",
                        "status" => "status",
                        _ => "updated_at",
                    },
                )
            }
            _ => return Err("无效的管理列表".into()),
        };
        let admin_ids = serde_json::to_string(administrators)?;
        let now = super::accounts::ordered_time(Utc::now());
        let direction = if input.sort.is_empty() && model != "whitelist" || input.desc {
            "DESC"
        } else {
            "ASC"
        };
        let mut tx = self.db.begin().await?;
        if let Some(id) = input.id {
            let sql = format!("SELECT {projection} FROM ({source}) WHERE id=?3");
            let row: Option<String> = sqlx::query_scalar(&sql)
                .bind(&admin_ids)
                .bind(&now)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(match row {
                Some(row) => serde_json::from_str(&row)?,
                None => Value::Null,
            });
        }
        let query = input.q.trim().to_lowercase();
        // Bind slots are shared by all model-specific projections.
        let where_sql = format!("FROM ({source}) WHERE ?3 IS NULL AND {filter} AND ?6 IS NOT NULL");
        let count = format!("SELECT count(*) {where_sql}");
        let total: i64 = sqlx::query_scalar(&count)
            .bind(&admin_ids)
            .bind(&now)
            .bind(None::<String>)
            .bind(&query)
            .bind(&input.filter)
            .bind(&input.role)
            .fetch_one(&mut *tx)
            .await?;
        let size = 20;
        let page = input.page.clamp(1, ((total + size - 1) / size).max(1));
        let select = format!(
            "SELECT {projection} {where_sql} ORDER BY {sort} {direction},id LIMIT ?7 OFFSET ?8"
        );
        let rows: Vec<String> = sqlx::query_scalar(&select)
            .bind(admin_ids)
            .bind(now)
            .bind(None::<String>)
            .bind(query)
            .bind(input.filter)
            .bind(input.role)
            .bind(size)
            .bind((page - 1) * size)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        let records: Vec<Value> = rows
            .iter()
            .map(|row| serde_json::from_str(row))
            .collect::<Result<_, _>>()?;
        Ok(json!({"records":records,"total":total,"page":page,"page_size":size}))
    }

    pub(crate) async fn admin_related(
        &self,
        users: &[Uuid],
        jobs: &[Uuid],
    ) -> Result<Value, StoreError> {
        let users = serde_json::to_string(users)?;
        let jobs = serde_json::to_string(jobs)?;
        let names: Vec<(String, String)> = sqlx::query_as(
            "SELECT id,username FROM users WHERE id IN (SELECT value FROM json_each(?))",
        )
        .bind(&users)
        .fetch_all(&self.db)
        .await?;
        let kinds: Vec<(String,String)> = sqlx::query_as(
            "SELECT id,'notes' FROM pkuvideo_jobs WHERE id IN (SELECT value FROM json_each(?1))
             UNION ALL SELECT id,'replay' FROM pkuvideo_fw_jobs WHERE id IN (SELECT value FROM json_each(?1))
             UNION ALL SELECT id,'translation' FROM translation_jobs WHERE id IN (SELECT value FROM json_each(?1))",
        ).bind(&jobs).fetch_all(&self.db).await?;
        Ok(
            json!({"users":names.into_iter().collect::<HashMap<_,_>>(),"jobs":kinds.into_iter().collect::<HashMap<_,_>>()}),
        )
    }

    pub(crate) async fn change_whitelist(
        &self,
        old: Option<String>,
        email: Option<String>,
        remove: Vec<String>,
    ) -> Result<(), crate::error::ApiError> {
        use crate::error::ApiError;
        let db_error = |error: sqlx::Error| ApiError::Internal(error.to_string());
        let mut tx = self
            .db
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(db_error)?;
        if let Some(email) = email {
            if let Some(ref old) = old {
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM registration_whitelist WHERE email=?)",
                )
                .bind(old)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_error)?;
                if !exists {
                    return Err(ApiError::NotFound);
                }
            }
            if old.as_deref() != Some(email.as_str()) {
                let inserted = sqlx::query("INSERT INTO registration_whitelist(email) VALUES (?) ON CONFLICT(email) DO NOTHING")
                    .bind(email).execute(&mut *tx).await.map_err(db_error)?;
                if inserted.rows_affected() == 0 {
                    return Err(ApiError::Conflict("此邮箱已在白名单中。".into()));
                }
                if let Some(old) = old {
                    sqlx::query("DELETE FROM registration_whitelist WHERE email=?")
                        .bind(old)
                        .execute(&mut *tx)
                        .await
                        .map_err(db_error)?;
                }
            }
        } else {
            let emails = serde_json::to_string(&remove)
                .map_err(|error| ApiError::Internal(error.to_string()))?;
            sqlx::query("DELETE FROM registration_whitelist WHERE email IN (SELECT value FROM json_each(?))")
                .bind(emails).execute(&mut *tx).await.map_err(db_error)?;
        }
        tx.commit().await.map_err(db_error)?;
        Ok(())
    }
}
