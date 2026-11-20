use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use ring::{
    aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM},
    rand::{SecureRandom, SystemRandom},
};
use std::{io::Write, path::Path};

const KEY_ID: i32 = 1;
const KEY_ENV: &str = "ASSISTANT_PROVIDER_KEY";
const PROVIDERS: [&str; 5] = ["openai", "anthropic", "aliyun", "deepseek", "mimo"];

pub(super) struct ProviderKeys {
    path: PathBuf,
    key: tokio::sync::OnceCell<LessSafeKey>,
}

impl ProviderKeys {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("provider-key.bin"),
            key: tokio::sync::OnceCell::new(),
        }
    }

    async fn get(&self, create: bool) -> Result<&LessSafeKey, StoreError> {
        self.key
            .get_or_try_init(|| async {
                let material = match std::env::var(KEY_ENV) {
                    Ok(value) => STANDARD
                        .decode(value.trim())
                        .map_err(|_| "供应商主密钥 Base64 格式无效")?,
                    Err(std::env::VarError::NotPresent) => {
                        match tokio::fs::read(&self.path).await {
                            Ok(bytes) => bytes,
                            Err(error)
                                if create && error.kind() == std::io::ErrorKind::NotFound =>
                            {
                                let path = self.path.clone();
                                tokio::task::spawn_blocking(move || create_key_file(&path))
                                    .await??
                            }
                            Err(error) => {
                                return Err(format!(
                                    "无法读取供应商主密钥 {}: {error}",
                                    self.path.display()
                                )
                                .into())
                            }
                        }
                    }
                    Err(_) => return Err("供应商主密钥环境变量无法读取".into()),
                };
                let key = UnboundKey::new(&AES_256_GCM, &material)
                    .map_err(|_| "供应商主密钥必须为 32 字节")?;
                Ok(LessSafeKey::new(key))
            })
            .await
    }

    async fn encrypt(
        &self,
        user: Uuid,
        provider: &str,
        secret: &str,
    ) -> Result<Vec<u8>, StoreError> {
        let key = self.get(true).await?;
        let mut nonce = [0; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| "无法生成随机数")?;
        let mut bytes = secret.as_bytes().to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            context(user, provider),
            &mut bytes,
        )
        .map_err(|_| "供应商密钥加密失败")?;
        let mut ciphertext = nonce.to_vec();
        ciphertext.append(&mut bytes);
        Ok(ciphertext)
    }

    async fn decrypt(
        &self,
        user: Uuid,
        provider: &str,
        key_id: i32,
        mut bytes: Vec<u8>,
    ) -> Result<String, StoreError> {
        if key_id != KEY_ID || bytes.len() < 12 {
            return Err("供应商密钥格式不受支持".into());
        }
        let key = self.get(false).await?;
        let nonce =
            Nonce::try_assume_unique_for_key(&bytes[..12]).map_err(|_| "供应商密钥格式无效")?;
        let clear = key
            .open_in_place(nonce, context(user, provider), &mut bytes[12..])
            .map_err(|_| "供应商密钥解密失败，请核对主密钥")?;
        Ok(std::str::from_utf8(clear)?.into())
    }

    pub async fn validate(&self, db: &SqlitePool) -> Result<(), StoreError> {
        let row: Option<(String, String, i32, Vec<u8>)> = sqlx::query_as(
            "SELECT user_id,provider,key_id,secret FROM user_provider_configs WHERE secret IS NOT NULL LIMIT 1",
        ).fetch_optional(db).await?;
        if let Some((user, provider, key_id, secret)) = row {
            self.decrypt(user.parse()?, &provider, key_id, secret)
                .await?;
        }
        Ok(())
    }
}

fn context(user: Uuid, provider: &str) -> Aad<String> {
    Aad::from(format!("assistant.provider.v1/{user}/{provider}"))
}

fn create_key_file(path: &Path) -> Result<Vec<u8>, StoreError> {
    let mut material = vec![0; 32];
    SystemRandom::new()
        .fill(&mut material)
        .map_err(|_| "无法生成供应商主密钥")?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Ok(std::fs::read(path)?)
        }
        Err(error) => return Err(error.into()),
    };
    file.write_all(&material)?;
    file.sync_all()?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(material)
}

#[derive(Default, PartialEq, Eq)]
struct ProviderConfig {
    model: Option<String>,
    secret: Option<String>,
}

fn take_providers(config: &mut FileUserConfig) -> [ProviderConfig; 5] {
    [
        ProviderConfig {
            model: None,
            secret: config.openai_api_key.take(),
        },
        ProviderConfig {
            model: None,
            secret: config.anthropic_api_key.take(),
        },
        ProviderConfig {
            model: config.aliyun_model.take(),
            secret: config.aliyun_api_key.take(),
        },
        ProviderConfig {
            model: config.deepseek_model.take(),
            secret: config.deepseek_api_key.take(),
        },
        ProviderConfig {
            model: config.mimo_model.take(),
            secret: config.mimo_api_key.take(),
        },
    ]
}

pub(super) async fn read_settings(
    connection: &mut sqlx::SqliteConnection,
    keys: &ProviderKeys,
    user: Uuid,
) -> Result<Option<FileUserConfig>, StoreError> {
    let config: Option<FileUserConfig> =
        read_entity(&mut *connection, "user_configs", user).await?;
    let Some(mut config) = config else {
        return Ok(None);
    };
    let rows: Vec<(String, Option<String>, Option<i32>, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT provider,model,key_id,secret FROM user_provider_configs WHERE user_id=?",
    )
    .bind(user.to_string())
    .fetch_all(connection)
    .await?;
    for (provider, model, key_id, secret) in rows {
        let secret = if let Some(bytes) = secret {
            Some(
                keys.decrypt(user, &provider, key_id.ok_or("供应商密钥缺少版本")?, bytes)
                    .await?,
            )
        } else {
            None
        };
        match provider.as_str() {
            "openai" => config.openai_api_key = secret,
            "anthropic" => config.anthropic_api_key = secret,
            "aliyun" => (config.aliyun_model, config.aliyun_api_key) = (model, secret),
            "deepseek" => (config.deepseek_model, config.deepseek_api_key) = (model, secret),
            "mimo" => (config.mimo_model, config.mimo_api_key) = (model, secret),
            _ => return Err("供应商配置名称不受支持".into()),
        }
    }
    Ok(Some(config))
}

pub(super) async fn write_settings(
    connection: &mut sqlx::SqliteConnection,
    keys: &ProviderKeys,
    user: Uuid,
    mut config: FileUserConfig,
    mut previous: Option<FileUserConfig>,
) -> Result<(), StoreError> {
    if config.user_id != user {
        return Err("用户配置归属不一致".into());
    }
    let records = take_providers(&mut config);
    let old = previous.as_mut().map(take_providers).unwrap_or_default();
    write_entity(&mut *connection, "user_configs", user, &config).await?;
    for ((provider, new), old) in PROVIDERS.into_iter().zip(records).zip(old) {
        if new == old {
            continue;
        }
        if new == ProviderConfig::default() {
            sqlx::query("DELETE FROM user_provider_configs WHERE user_id=? AND provider=?")
                .bind(user.to_string())
                .bind(provider)
                .execute(&mut *connection)
                .await?;
        } else if new.secret == old.secret && old != ProviderConfig::default() {
            sqlx::query("UPDATE user_provider_configs SET model=? WHERE user_id=? AND provider=?")
                .bind(new.model)
                .bind(user.to_string())
                .bind(provider)
                .execute(&mut *connection)
                .await?;
        } else {
            let secret = match new.secret {
                Some(secret) => Some(keys.encrypt(user, provider, &secret).await?),
                None => None,
            };
            let key_id = secret.as_ref().map(|_| KEY_ID);
            sqlx::query("INSERT INTO user_provider_configs(user_id,provider,model,key_id,secret) VALUES (?,?,?,?,?)
                ON CONFLICT(user_id,provider) DO UPDATE SET model=excluded.model,key_id=excluded.key_id,secret=excluded.secret")
                .bind(user.to_string()).bind(provider).bind(new.model).bind(key_id).bind(secret)
                .execute(&mut *connection).await?;
        }
    }
    Ok(())
}
