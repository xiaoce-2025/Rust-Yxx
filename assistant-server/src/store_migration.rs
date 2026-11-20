use super::*;
use std::path::Path;

const VERSION: i64 = 2;
const SCHEMA: &str = include_str!("store_schema.sql");

async fn legacy<T: DeserializeOwned + Default>(
    tx: &mut Transaction<'_, Sqlite>,
    dir: &Path,
    name: &str,
) -> Result<T, StoreError> {
    let stored: Option<String> = sqlx::query_scalar("SELECT payload FROM collections WHERE name=?")
        .bind(name)
        .fetch_optional(&mut **tx)
        .await?;
    if let Some(payload) = stored {
        return serde_json::from_str(&payload)
            .map_err(|error| format!("无法迁移旧集合 {name}: {error}").into());
    }
    let path = dir.join(format!("{name}.json"));
    match tokio::fs::read_to_string(&path).await {
        Ok(payload) if name == "wechat_channels" => {
            let channels: Vec<FileUserWeChatChannel> = serde_json::from_str(&payload)?;
            let mut by_user = HashMap::new();
            for channel in channels {
                if by_user.insert(channel.user_id, channel).is_some() {
                    return Err("旧微信频道包含重复用户".into());
                }
            }
            Ok(serde_json::from_value(serde_json::to_value(by_user)?)?)
        }
        Ok(payload) => serde_json::from_str(&payload)
            .map_err(|error| format!("无法迁移旧文件 {}: {error}", path.display()).into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error.into()),
    }
}

async fn import_rows<T, Key>(
    tx: &mut Transaction<'_, Sqlite>,
    dir: &Path,
    legacy_name: &str,
    table: &'static str,
    key: impl Fn(&T) -> Key,
) -> Result<(), StoreError>
where
    T: DeserializeOwned + Serialize,
    Key: ToString,
{
    let payload: serde_json::Value = legacy(tx, dir, legacy_name).await?;
    let records = match payload {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::Array(rows) => rows,
        serde_json::Value::Object(rows) => rows.into_values().collect(),
        _ => return Err(format!("旧集合 {legacy_name} 格式不正确").into()),
    };
    let sql = format!("INSERT INTO {table}(id,payload) VALUES (?,?)");
    for value in records {
        let record: T = serde_json::from_value(value)
            .map_err(|error| format!("旧集合 {legacy_name} 包含无效记录: {error}"))?;
        sqlx::query(&sql)
            .bind(key(&record).to_string())
            .bind(serde_json::to_string(&record)?)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("迁移 {legacy_name} 失败: {error}"))?;
    }
    Ok(())
}

pub(super) async fn migrate(
    db: &SqlitePool,
    dir: &Path,
    keys: &provider_config::ProviderKeys,
) -> Result<(), StoreError> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS store_migrations (version INTEGER PRIMARY KEY)")
        .execute(&mut *tx)
        .await?;
    let version: i64 = sqlx::query_scalar("SELECT coalesce(max(version),0) FROM store_migrations")
        .fetch_one(&mut *tx)
        .await?;
    if version > VERSION {
        return Err(format!("数据库结构版本 {version} 高于程序支持的 {VERSION}").into());
    }
    if version == VERSION {
        tx.commit().await?;
        return Ok(());
    }
    if version == 0 {
        migrate_entities(&mut tx, dir, keys).await?;
        sqlx::query("INSERT INTO store_migrations(version) VALUES (1)")
            .execute(&mut *tx)
            .await?;
    }
    sections::migrate(&mut tx).await?;
    sqlx::query("INSERT INTO store_migrations(version) VALUES (?)")
        .bind(VERSION)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn migrate_entities(
    tx: &mut Transaction<'_, Sqlite>,
    dir: &Path,
    keys: &provider_config::ProviderKeys,
) -> Result<(), StoreError> {
    sqlx::query("CREATE TABLE IF NOT EXISTS collections (name TEXT PRIMARY KEY NOT NULL,payload TEXT NOT NULL CHECK(json_valid(payload)))")
        .execute(&mut **tx).await?;
    sqlx::raw_sql(SCHEMA).execute(&mut **tx).await?;
    import_rows(tx, dir, "users", "users", |r: &FileUser| r.id).await?;
    let configs: HashMap<Uuid, FileUserConfig> = legacy(tx, dir, "configs").await?;
    for (user, config) in configs {
        provider_config::write_settings(tx, keys, user, config, None).await?;
    }
    import_rows(
        tx,
        dir,
        "conversations",
        "conversations",
        |r: &FileConversation| r.id,
    )
    .await?;
    import_rows(
        tx,
        dir,
        "messages",
        "messages",
        |r: &FileConversationMessage| r.id,
    )
    .await?;
    import_rows(
        tx,
        dir,
        "wechat_bindings",
        "wechat_bindings",
        |r: &FileUserWeChatBinding| r.id,
    )
    .await?;
    import_rows(
        tx,
        dir,
        "wechat_channels",
        "wechat_channels",
        |r: &FileUserWeChatChannel| r.user_id,
    )
    .await?;
    import_rows(
        tx,
        dir,
        "oauth_states",
        "oauth_states",
        |r: &FileWeChatOAuthState| r.state.clone(),
    )
    .await?;
    import_rows(
        tx,
        dir,
        "pkuvideo_jobs",
        "pkuvideo_jobs",
        |r: &PkuVideoJob| r.id,
    )
    .await?;
    import_rows(
        tx,
        dir,
        "pkuvideo_fw_jobs",
        "pkuvideo_fw_jobs",
        |r: &PkuVideoFwJob| r.id,
    )
    .await?;
    let sessions: HashMap<Uuid, FileUserSession> = legacy(tx, dir, "sessions").await?;
    for (id, session) in sessions {
        if id != session.session_id {
            return Err("旧会话集合中的标识与会话记录不一致".into());
        }
        write_session(tx, &session).await?;
    }
    let links: HashMap<String, Uuid> = legacy(tx, dir, "oidc_identities").await?;
    for (identity, user) in links {
        let (issuer, subject) = identity.split_once('\0').ok_or("旧 OIDC 标识格式不正确")?;
        sqlx::query("INSERT INTO oidc_identities(issuer,subject,user_id) VALUES (?,?,?)")
            .bind(issuer)
            .bind(subject)
            .bind(user.to_string())
            .execute(&mut **tx)
            .await?;
    }
    let whitelist = match tokio::fs::read_to_string(dir.join("whitelist.json")).await {
        Ok(payload) => serde_json::from_str::<RegistrationWhitelist>(&payload)?.allowed_emails,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    replace_whitelist(tx, whitelist).await?;
    sqlx::query("INSERT INTO store_documents SELECT name,payload FROM collections WHERE name >= 'sidecar/' AND name < 'sidecar0'")
        .execute(&mut **tx).await?;
    // Move recognized translation manifests without ever falling back to an old
    // manifest that has already been masked by an explicit deletion.
    let manifests: Vec<(String, String)> = sqlx::query_as(
        "SELECT name,payload FROM store_documents
         WHERE name >= 'sidecar/data/pkuvideo_fw_translations/' AND name < 'sidecar/data/pkuvideo_fw_translations0'
           AND payload IS NOT NULL AND name LIKE '%/manifest.json'",
    ).fetch_all(&mut **tx).await?;
    for (name, payload) in manifests {
        let job: crate::services::video_translation::TranslationJob =
            serde_json::from_str(&payload)?;
        let expected = format!(
            "sidecar/data/pkuvideo_fw_translations/{}/manifest.json",
            job.id
        );
        if name != expected || job.version != 1 {
            return Err("旧翻译记录的标识或版本不一致".into());
        }
        write_entity(tx, "translation_jobs", job.id, &job).await?;
        sqlx::query("UPDATE store_documents SET payload=NULL WHERE name=?")
            .bind(name)
            .execute(&mut **tx)
            .await?;
    }

    Ok(())
}
