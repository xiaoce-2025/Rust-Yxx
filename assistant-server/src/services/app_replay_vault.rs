//! Server-side encrypted shared binding credentials and persistent App sessions.
use crate::store::FileStore;
use base64::{engine::general_purpose::STANDARD, Engine};
use rsa::{
    pkcs8::{DecodePrivateKey, EncodePrivateKey},
    traits::PublicKeyParts,
    Oaep, RsaPrivateKey,
};
use serde::{de::DeserializeOwned, Serialize};
use sha2::Sha256;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};
use uuid::Uuid;

static VAULT_LOCK: Mutex<()> = Mutex::new(());
fn root() -> PathBuf {
    PathBuf::from("./data/app_replay")
}
fn key(dir: &Path) -> Result<RsaPrivateKey, String> {
    fs::create_dir_all(dir).map_err(|_| "创建 App 凭据目录失败")?;
    let path = dir.join("vault-key.pem");
    if path.exists() {
        return RsaPrivateKey::from_pkcs8_pem(
            &fs::read_to_string(path).map_err(|_| "读取 App 凭据密钥失败")?,
        )
        .map_err(|_| "App 凭据密钥损坏".into());
    }
    let key =
        RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).map_err(|_| "生成 App 凭据密钥失败")?;
    let pem = key
        .to_pkcs8_pem(Default::default())
        .map_err(|_| "保存 App 凭据密钥失败")?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    options
        .open(path)
        .and_then(|mut f| {
            f.write_all(pem.as_bytes())?;
            f.sync_all()
        })
        .map_err(|_| "保存 App 凭据密钥失败")?;
    Ok(key)
}

pub(super) async fn save<T: Serialize>(store: &FileStore, user: Uuid, kind: &str, value: &T) -> Result<(), String> {
    save_in(store, &root(), user, kind, value).await
}

async fn save_in<T: Serialize>(store: &FileStore, dir: &Path, user: Uuid, kind: &str, value: &T) -> Result<(), String> {
    let payload = encrypt(dir, value)?;
    store.save_sidecar(&dir.join(format!("{user}-{kind}.json")), &payload).await
}

fn encrypt<T: Serialize>(dir: &Path, value: &T) -> Result<String, String> {
    let _lock = VAULT_LOCK.lock().map_err(|_| "App 凭据锁异常")?;
    let key = key(dir)?;
    let public = key.to_public_key();
    let bytes = serde_json::to_vec(value).map_err(|_| "编码 App 凭据失败")?;
    let size = public.size() - 2 * 32 - 2;
    let blocks: Vec<String> = bytes
        .chunks(size)
        .map(|chunk| {
            public
                .encrypt(&mut rand::rngs::OsRng, Oaep::new::<Sha256>(), chunk)
                .map(|b| STANDARD.encode(b))
                .map_err(|_| "加密 App 凭据失败")
        })
        .collect::<Result<_, _>>()?;
    serde_json::to_string(&blocks).map_err(|_| "编码 App 凭据失败".into())
}

pub(super) async fn load<T: DeserializeOwned>(store: &FileStore, user: Uuid, kind: &str) -> Result<Option<T>, String> {
    load_in(store, &root(), user, kind).await
}

async fn load_in<T: DeserializeOwned>(store: &FileStore, dir: &Path, user: Uuid, kind: &str) -> Result<Option<T>, String> {
    let path = dir.join(format!("{user}-{kind}.json"));
    let Some(blocks) = store.load_sidecar::<Option<Vec<String>>>(&path).await?.flatten() else {
        return Ok(None);
    };
    let _lock = VAULT_LOCK.lock().map_err(|_| "App 凭据锁异常")?;
    let key = key(dir)?;
    let mut bytes = Vec::new();
    for block in blocks {
        bytes.extend(
            key.decrypt(
                Oaep::new::<Sha256>(),
                &STANDARD.decode(block).map_err(|_| "App 凭据格式无效")?,
            )
            .map_err(|_| "解密 App 凭据失败")?,
        );
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| "App 凭据数据无效".into())
}
pub(super) async fn remove(store: &FileStore, user: Uuid, kind: &str) -> Result<(), String> {
    let path = root().join(format!("{user}-{kind}.json"));
    // Record the deletion before removing any legacy file so a retry cannot reimport it.
    store.save_sidecar(&path, "null").await?;
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("清除 App 凭据失败".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn encrypted_credentials_and_session_survive_reload_without_plaintext() {
        let dir = tempdir::TempDir::new_in("target", "app-vault").unwrap();
        let store = FileStore::new(dir.path().into()).await.unwrap();
        let id = Uuid::new_v4();
        let value = serde_json::json!({"password":"private-password","user_agent":"same-agent","_token":"a".repeat(600)});
        save_in(&store, dir.path(), id, "session", &value).await.unwrap();
        let raw = store.stored_sidecar(&dir.path().join(format!("{id}-session.json"))).await.unwrap().unwrap();
        assert!(!raw.contains("private-password"));
        assert_eq!(
            load_in::<serde_json::Value>(&store, dir.path(), id, "session").await.unwrap(),
            Some(value)
        );
        assert!(
            load_in::<serde_json::Value>(&store, dir.path(), Uuid::new_v4(), "session").await
                .unwrap()
                .is_none()
        );
    }
}
