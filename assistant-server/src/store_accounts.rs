use super::*;
use crate::{auth::CurrentUser, error::ApiError};
use chrono::SecondsFormat;

pub(super) fn ordered_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub(super) async fn write_session(
    connection: &mut sqlx::SqliteConnection,
    session: &FileUserSession,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO sessions(id,user_id,expires_at,created_at,user_agent,ip_address) VALUES (?,?,?,?,?,?)
         ON CONFLICT(id) DO UPDATE SET user_id=excluded.user_id,expires_at=excluded.expires_at,
             created_at=excluded.created_at,user_agent=excluded.user_agent,ip_address=excluded.ip_address",
    )
    .bind(session.session_id.to_string())
    .bind(session.user_id.to_string())
    .bind(ordered_time(session.expires_at))
    .bind(ordered_time(session.created_at))
    .bind(&session.user_agent)
    .bind(&session.ip_address)
    .execute(connection)
    .await?;
    Ok(())
}

pub(super) async fn replace_whitelist(
    connection: &mut sqlx::SqliteConnection,
    emails: Vec<String>,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM registration_whitelist")
        .execute(&mut *connection)
        .await?;
    for email in emails {
        sqlx::query(
            "INSERT INTO registration_whitelist(email) VALUES (?) ON CONFLICT(email) DO NOTHING",
        )
        .bind(email)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

async fn allowed(
    connection: &mut sqlx::SqliteConnection,
    email: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM registration_whitelist WHERE email=?)")
        .bind(email)
        .fetch_one(connection)
        .await
}

fn account_error(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(%error, "账户数据库操作失败");
    ApiError::Internal("账户数据库操作失败".into())
}

async fn insert_account(
    connection: &mut sqlx::SqliteConnection,
    user: &FileUser,
) -> Result<(), StoreError> {
    sqlx::query("INSERT INTO users(id,payload) VALUES (?,?)")
        .bind(user.id.to_string())
        .bind(serde_json::to_string(user)?)
        .execute(&mut *connection)
        .await?;
    sqlx::query("INSERT INTO note_credit_accounts(user_id,balance,updated_at) VALUES (?,?,?) ON CONFLICT(user_id) DO NOTHING")
        .bind(user.id.to_string()).bind(user.processing_quota).bind(Utc::now().to_rfc3339())
        .execute(&mut *connection).await?;
    Ok(())
}

impl FileStore {
    pub async fn authenticated_user(
        &self,
        session_id: Uuid,
    ) -> Result<Option<CurrentUser>, StoreError> {
        let account: Option<(String, String, String)> = sqlx::query_as(
            "SELECT users.id,users.username,users.email
             FROM sessions JOIN users ON users.id=sessions.user_id
             WHERE sessions.id=? AND sessions.expires_at>=? AND users.active=1",
        )
        .bind(session_id.to_string())
        .bind(ordered_time(Utc::now()))
        .fetch_optional(&self.db)
        .await?;
        account
            .map(|(id, username, email)| {
                Ok(CurrentUser {
                    id: id.parse()?,
                    username,
                    email,
                })
            })
            .transpose()
    }

    #[cfg(test)]
    pub async fn get_session(&self, session_id: Uuid) -> Option<FileUserSession> {
        let (user, expires_at, created_at, user_agent, ip_address): (
            String,
            DateTime<Utc>,
            DateTime<Utc>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT user_id,expires_at,created_at,user_agent,ip_address FROM sessions WHERE id=?",
        )
        .bind(session_id.to_string())
        .fetch_optional(&self.db)
        .await
        .ok()??;
        Some(FileUserSession {
            session_id,
            user_id: user.parse().ok()?,
            expires_at,
            created_at,
            user_agent,
            ip_address,
        })
    }

    pub async fn insert_session(&self, session: FileUserSession) -> Result<(), StoreError> {
        self.insert_session_record(session).await
    }

    pub async fn delete_session(&self, session_id: Uuid) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM sessions WHERE id=?")
            .bind(session_id.to_string())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn prune_sessions(&self) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM sessions WHERE id IN (SELECT id FROM sessions WHERE expires_at<? ORDER BY expires_at LIMIT 1000)")
            .bind(ordered_time(Utc::now())).execute(&self.db).await?;
        Ok(())
    }

    pub async fn admin_revoke_sessions(
        &self,
        user_id: Uuid,
        audit_id: Option<i64>,
    ) -> Result<usize, StoreError> {
        let mut tx = self.begin_note_transaction().await?;
        let count = sqlx::query("DELETE FROM sessions WHERE user_id=?")
            .bind(user_id.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected() as usize;
        operations::audit_detail(
            &mut tx,
            audit_id,
            serde_json::json!({"revoked_sessions":count}),
        )
        .await?;
        tx.commit().await?;
        Ok(count)
    }

    pub async fn admin_session_counts(&self) -> HashMap<Uuid, usize> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT user_id,count(*) FROM sessions WHERE expires_at>? GROUP BY user_id",
        )
        .bind(ordered_time(Utc::now()))
        .fetch_all(&self.db)
        .await
        .unwrap_or_else(|error| {
            tracing::error!(%error, "读取会话数量失败");
            Vec::new()
        });
        rows.into_iter()
            .filter_map(|(id, n)| Some((id.parse().ok()?, n as usize)))
            .collect()
    }

    pub async fn admin_whitelist(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT email FROM registration_whitelist ORDER BY email")
            .fetch_all(&self.db)
            .await
            .unwrap_or_else(|error| {
                tracing::error!(%error, "读取白名单失败");
                Vec::new()
            })
    }

    pub async fn admin_save_whitelist(&self, emails: Vec<String>) -> Result<(), StoreError> {
        let mut tx = self.begin_note_transaction().await?;
        replace_whitelist(&mut tx, emails).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn is_email_in_whitelist(&self, email: &str) -> bool {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM registration_whitelist WHERE email=?)")
            .bind(email)
            .fetch_one(&self.db)
            .await
            .unwrap_or_else(|error| {
                tracing::error!(%error, "查询白名单失败");
                false
            })
    }

    pub async fn admin_users(&self) -> Vec<FileUser> {
        let rows: Result<Vec<(String,i64)>,_> = sqlx::query_as(
            "SELECT users.payload,accounts.balance FROM users JOIN note_credit_accounts accounts ON accounts.user_id=users.id",
        ).fetch_all(&self.db).await;
        match rows {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|(payload, balance)| project_user(payload, balance))
                .collect(),
            Err(error) => {
                tracing::error!(%error, "读取用户列表失败");
                Vec::new()
            }
        }
    }

    pub async fn admin_update_user(
        &self,
        id: Uuid,
        active: bool,
        storage: i32,
        priority: u32,
        audit_id: Option<i64>,
    ) -> Result<(), ApiError> {
        let mut tx = self.begin_note_transaction().await.map_err(account_error)?;
        let mut user: FileUser = read_entity(&mut *tx, "users", id)
            .await
            .map_err(account_error)?
            .ok_or(ApiError::NotFound)?;
        let before = serde_json::json!({"is_active":user.is_active,"storage_quota":user.storage_quota,"processing_priority":user.processing_priority});
        user.is_active = active;
        user.storage_quota = storage;
        user.processing_priority = priority;
        user.updated_at = Utc::now();
        write_entity(&mut tx, "users", id, &user)
            .await
            .map_err(account_error)?;
        let detail = serde_json::json!({"before":before,"after":{"is_active":active,"storage_quota":storage,"processing_priority":priority}});
        operations::audit_detail(&mut tx, audit_id, detail)
            .await
            .map_err(account_error)?;
        tx.commit().await.map_err(account_error)?;
        Ok(())
    }

    async fn user_by(&self, column: &'static str, value: String) -> Option<FileUser> {
        let sql = format!(
            "SELECT users.payload,accounts.balance FROM users JOIN note_credit_accounts accounts ON accounts.user_id=users.id
             WHERE users.{column}=? ORDER BY users.id LIMIT 1",
        );
        match sqlx::query_as::<_, (String, i64)>(&sql)
            .bind(value)
            .fetch_optional(&self.db)
            .await
        {
            Ok(row) => row.and_then(|(payload, balance)| project_user(payload, balance)),
            Err(error) => {
                tracing::error!(%error, "读取用户失败");
                None
            }
        }
    }

    pub async fn get_user(&self, user: Uuid) -> Option<FileUser> {
        self.user_by("id", user.to_string()).await
    }

    pub async fn get_user_by_email(&self, email: &str) -> Option<FileUser> {
        self.user_by("email", email.into()).await
    }

    pub async fn get_user_by_username(&self, username: &str) -> Option<FileUser> {
        self.user_by("username", username.into()).await
    }

    pub async fn insert_user(&self, user: FileUser) -> Result<(), StoreError> {
        let mut tx = self.begin_note_transaction().await?;
        insert_account(&mut tx, &user).await?;
        tx.commit().await?;
        Ok(())
    }

    /// A verified OIDC identity still needs this application's whitelist on every login.
    pub async fn resolve_oidc_user(
        &self,
        issuer: &str,
        subject: &str,
        email: &str,
        username: &str,
    ) -> Result<FileUser, ApiError> {
        let issuer = issuer.trim_end_matches('/');
        // Hash outside the write transaction. Existing accounts avoid the hash.
        let candidate_hash = if self.get_user_by_email(email).await.is_none() {
            Some(crate::auth::hash_password(&Uuid::new_v4().to_string())?)
        } else {
            None
        };
        let mut tx = self.begin_note_transaction().await.map_err(account_error)?;
        if !allowed(&mut tx, email).await.map_err(account_error)? {
            return Err(ApiError::Forbidden(
                "统一认证已通过，但该邮箱不在小助手白名单中".into(),
            ));
        }
        let linked: Option<String> =
            sqlx::query_scalar("SELECT user_id FROM oidc_identities WHERE issuer=? AND subject=?")
                .bind(issuer)
                .bind(subject)
                .fetch_optional(&mut *tx)
                .await
                .map_err(account_error)?;
        let user = if let Some(ref id) = linked {
            let user: FileUser = read_entity(&mut *tx, "users", id)
                .await
                .map_err(account_error)?
                .ok_or_else(|| ApiError::Forbidden("关联账户不存在，请联系管理员".into()))?;
            if user.email != email {
                return Err(ApiError::Forbidden(
                    "统一账户邮箱已变更，请联系管理员重新关联".into(),
                ));
            }
            user
        } else if let Some(user) = account_by_email(&mut tx, email)
            .await
            .map_err(account_error)?
        {
            let already_linked: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM oidc_identities WHERE issuer=? AND user_id=?)",
            )
            .bind(issuer)
            .bind(user.id.to_string())
            .fetch_one(&mut *tx)
            .await
            .map_err(account_error)?;
            if already_linked {
                return Err(ApiError::Forbidden(
                    "此邮箱已关联其他统一认证账户，请联系管理员".into(),
                ));
            }
            user
        } else {
            let id = Uuid::new_v4();
            let now = Utc::now();
            let user = FileUser {
                id,
                username: available_username(&mut tx, username, id)
                    .await
                    .map_err(account_error)?,
                email: email.into(),
                password_hash: candidate_hash
                    .ok_or_else(|| ApiError::Internal("无法创建统一认证账户".into()))?,
                is_active: true,
                created_at: now,
                updated_at: now,
                storage_quota: 3,
                processing_quota: 1,
                processing_priority: 5,
                iaaa_encrypted_password: None,
                iaaa_public_key: None,
                iaaa_key_version: None,
                iaaa_username: None,
            };
            insert_account(&mut tx, &user)
                .await
                .map_err(account_error)?;
            user
        };
        if !user.is_active {
            return Err(ApiError::Forbidden("小助手账户已停用".into()));
        }
        if linked.is_none() {
            sqlx::query("INSERT INTO oidc_identities(issuer,subject,user_id) VALUES (?,?,?)")
                .bind(issuer)
                .bind(subject)
                .bind(user.id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(account_error)?;
        }
        tx.commit().await.map_err(account_error)?;
        Ok(user)
    }

    pub async fn resolve_iaaa_user(
        &self,
        mut candidate: FileUser,
    ) -> Result<Option<FileUser>, StoreError> {
        let mut tx = self.begin_note_transaction().await?;
        if let Some(user) = account_by_email(&mut tx, &candidate.email).await? {
            return Ok(Some(user));
        }
        if !allowed(&mut tx, &candidate.email).await? {
            return Ok(None);
        }
        candidate.username = available_username(&mut tx, &candidate.username, candidate.id).await?;
        insert_account(&mut tx, &candidate).await?;
        tx.commit().await?;
        Ok(Some(candidate))
    }

    async fn update_user_record(
        &self,
        id: Uuid,
        update: impl FnOnce(&mut FileUser) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut tx = self.begin_note_transaction().await?;
        if let Some(mut user) = read_entity(&mut *tx, "users", id).await? {
            update(&mut user)?;
            user.updated_at = Utc::now();
            write_entity(&mut tx, "users", id, &user).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn increase_storage_quota(
        &self,
        user_id: Uuid,
        amount: i32,
    ) -> Result<(), StoreError> {
        self.update_user_record(user_id, |user| {
            user.storage_quota = user
                .storage_quota
                .checked_add(amount)
                .ok_or("StorageOverflow")?;
            Ok(())
        })
        .await
    }

    pub async fn decrease_storage_quota(
        &self,
        user_id: Uuid,
        amount: i32,
    ) -> Result<(), StoreError> {
        self.update_user_record(user_id, |user| {
            user.storage_quota = user.storage_quota.saturating_sub(amount).max(0);
            Ok(())
        })
        .await
    }

    pub async fn update_iaaa_binding(
        &self,
        user_id: Uuid,
        encrypted_password: String,
        public_key: String,
        key_version: String,
        username: String,
    ) -> Result<(), StoreError> {
        self.update_user_record(user_id, |user| {
            user.iaaa_encrypted_password = Some(encrypted_password);
            user.iaaa_public_key = Some(public_key);
            user.iaaa_key_version = Some(key_version);
            user.iaaa_username = Some(username);
            Ok(())
        })
        .await
    }

    pub async fn remove_iaaa_binding(&self, user_id: Uuid) -> Result<(), StoreError> {
        self.update_user_record(user_id, |user| {
            user.iaaa_encrypted_password = None;
            user.iaaa_public_key = None;
            user.iaaa_key_version = None;
            user.iaaa_username = None;
            Ok(())
        })
        .await
    }

    pub async fn get_config(&self, user_id: Uuid) -> Result<Option<FileUserConfig>, StoreError> {
        let mut connection = self.db.acquire().await?;
        provider_config::read_settings(&mut connection, &self.provider_keys, user_id).await
    }

    pub async fn update_config(
        &self,
        user_id: Uuid,
        update: impl FnOnce(Option<FileUserConfig>) -> FileUserConfig,
    ) -> std::io::Result<()> {
        let result: Result<(), StoreError> = async {
            let mut tx = self.begin_note_transaction().await?;
            let previous =
                provider_config::read_settings(&mut tx, &self.provider_keys, user_id).await?;
            let config = update(previous.clone());
            provider_config::write_settings(
                &mut tx,
                &self.provider_keys,
                user_id,
                config,
                previous,
            )
            .await?;
            tx.commit().await?;
            Ok(())
        }
        .await;
        result.map_err(std::io::Error::other)
    }
}

fn project_user(payload: String, balance: i64) -> Option<FileUser> {
    match serde_json::from_str::<FileUser>(&payload) {
        Ok(mut user) => {
            user.processing_quota = balance;
            Some(user)
        }
        Err(error) => {
            tracing::error!(%error, "解析用户记录失败");
            None
        }
    }
}

async fn account_by_email(
    connection: &mut sqlx::SqliteConnection,
    email: &str,
) -> Result<Option<FileUser>, StoreError> {
    let payload: Option<String> = sqlx::query_scalar("SELECT payload FROM users WHERE email=?")
        .bind(email)
        .fetch_optional(connection)
        .await?;
    Ok(payload.as_deref().map(serde_json::from_str).transpose()?)
}

async fn available_username(
    connection: &mut sqlx::SqliteConnection,
    requested: &str,
    id: Uuid,
) -> Result<String, StoreError> {
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users WHERE username=?)")
            .bind(requested)
            .fetch_one(connection)
            .await?;
    Ok(if exists {
        format!("{requested}-{id}")
    } else {
        requested.into()
    })
}
