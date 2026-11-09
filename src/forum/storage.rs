//! Local encrypted snapshots; source records are revalidated on load.
use super::*;
use chacha20poly1305::{
    aead::{Aead, Payload},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use std::{
    io::{Read, Write},
    path::Path,
};
const MAGIC: &[u8] = b"SPARK-FORUM-1\0";
const MAX_SNAPSHOT: u64 = 128 * 1024 * 1024;
impl Forum {
    pub fn save(&self, path: &Path, key: &[u8; 32]) -> Result<()> {
        let plaintext = zeroize::Zeroizing::new(postcard::to_allocvec(self)?);
        save_bytes(path, key, &plaintext)
    }
    pub fn load(path: &Path, key: &[u8; 32], network: Digest, admin: &[u8]) -> Result<Self> {
        let plaintext = zeroize::Zeroizing::new(load_bytes(path, key)?);
        let decoded: Forum = postcard::from_bytes(&plaintext)?;
        ensure!(
            decoded.network == network && decoded.admin == admin,
            "论坛快照与可信配置不符"
        );
        validate_snapshot(decoded)
    }
}
pub(super) fn validate_snapshot(decoded: Forum) -> Result<Forum> {
    let mut verified = Forum::new(decoded.network, decoded.admin)?;
    for (id, record) in decoded.records {
        ensure!(verified.insert_verified(record)? == id, "记录哈希不匹配");
    }
    // Hash ordering is not causal ordering: classify only after every verified record is loaded.
    verified.check_pending_budget()?;
    for (id, body) in decoded.bodies {
        ensure!(verified.put_body(body)? == id, "正文哈希不匹配");
    }
    Ok(verified)
}
pub(super) fn save_bytes(path: &Path, key: &[u8; 32], bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() as u64 <= MAX_SNAPSHOT - 128, "论坛快照过大");
    let mut nonce = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut nonce);
    let cipher = XChaCha20Poly1305::new(key.into());
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &bytes,
                aad: MAGIC,
            },
        )
        .map_err(|_| anyhow::anyhow!("论坛快照加密失败"))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(MAGIC)?;
    file.write_all(&nonce)?;
    file.write_all(&ciphertext)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub(super) fn load_bytes(path: &Path, key: &[u8; 32]) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    ensure!(file.metadata()?.len() <= MAX_SNAPSHOT, "论坛快照过大");
    let mut data = Vec::new();
    file.take(MAX_SNAPSHOT + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= MAX_SNAPSHOT
            && data.starts_with(MAGIC)
            && data.len() >= MAGIC.len() + 24 + 16,
        "论坛快照格式无效"
    );
    let split = MAGIC.len() + 24;
    let plaintext = XChaCha20Poly1305::new(key.into())
        .decrypt(
            XNonce::from_slice(&data[MAGIC.len()..split]),
            Payload {
                msg: &data[split..],
                aad: MAGIC,
            },
        )
        .map_err(|_| anyhow::anyhow!("论坛快照解密失败"))?;
    Ok(plaintext)
}
