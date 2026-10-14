//! Server-side encrypted shared binding credentials and persistent App sessions.
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

pub(super) fn save<T: Serialize>(user: Uuid, kind: &str, value: &T) -> Result<(), String> {
    save_in(&root(), user, kind, value)
}
fn save_in<T: Serialize>(dir: &Path, user: Uuid, kind: &str, value: &T) -> Result<(), String> {
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
    let path = dir.join(format!("{user}-{kind}.json"));
    super::lesson_storage::atomic_write(
        &path,
        &serde_json::to_vec(&blocks).map_err(|_| "编码 App 凭据失败")?,
    )
}
pub(super) fn load<T: DeserializeOwned>(user: Uuid, kind: &str) -> Result<Option<T>, String> {
    load_in(&root(), user, kind)
}
fn load_in<T: DeserializeOwned>(dir: &Path, user: Uuid, kind: &str) -> Result<Option<T>, String> {
    let _lock = VAULT_LOCK.lock().map_err(|_| "App 凭据锁异常")?;
    let path = dir.join(format!("{user}-{kind}.json"));
    if !path.exists() {
        return Ok(None);
    }
    let key = key(dir)?;
    let blocks: Vec<String> =
        serde_json::from_slice(&fs::read(path).map_err(|_| "读取 App 凭据失败")?)
            .map_err(|_| "App 凭据文件损坏")?;
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
pub(super) fn remove(user: Uuid, kind: &str) -> Result<(), String> {
    let _lock = VAULT_LOCK.lock().map_err(|_| "App 凭据锁异常")?;
    let path = root().join(format!("{user}-{kind}.json"));
    if path.exists() {
        fs::remove_file(path).map_err(|_| "清除 App 凭据失败")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encrypted_credentials_and_session_survive_reload_without_plaintext() {
        let dir = tempdir::TempDir::new_in("target", "app-vault").unwrap();
        let id = Uuid::new_v4();
        let value = serde_json::json!({"password":"private-password","user_agent":"same-agent","_token":"a".repeat(600)});
        save_in(dir.path(), id, "session", &value).unwrap();
        let raw = fs::read_to_string(dir.path().join(format!("{id}-session.json"))).unwrap();
        assert!(!raw.contains("private-password"));
        assert_eq!(
            load_in::<serde_json::Value>(dir.path(), id, "session").unwrap(),
            Some(value)
        );
        assert!(
            load_in::<serde_json::Value>(dir.path(), Uuid::new_v4(), "session")
                .unwrap()
                .is_none()
        );
    }
}
