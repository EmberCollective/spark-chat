use super::*;
use std::io::{Read, Write};
pub(super) const CHUNK: usize = 256 * 1024;
#[cfg(target_os = "android")]
const CACHE_LIMIT: u64 = 256_000_000;
#[cfg(not(target_os = "android"))]
const CACHE_LIMIT: u64 = 1_000_000_000;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttachmentInput {
    pub name: String,
    pub size: u64,
    pub hash: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Cached {
    pub size: u64,
    pub pinned: bool,
    pub used: u64,
}
pub(super) struct Download {
    pub hash: Digest,
    pub size: u64,
    pub received: u64,
    pub digest: Sha256,
    pub temporary: tempfile::TempDir,
    pub updated: Instant,
}
impl Worker {
    fn blobs(&self) -> PathBuf {
        self.path.parent().unwrap().join("objects")
    }
    fn blob_path(&self, hash: &Digest) -> PathBuf {
        self.blobs().join(hex(hash))
    }
    fn make_room(&mut self, bytes: u64) -> Result<()> {
        ensure!(bytes <= MAX_ATTACHMENT_BYTES, "附件超过 20 MB");
        let saved = self
            .saved
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("尚未加入论坛"))?;
        let mut used = saved.cached.values().map(|c| c.size).sum::<u64>()
            + self.downloads.values().map(|d| d.size).sum::<u64>();
        while used + bytes > CACHE_LIMIT {
            let next = saved
                .cached
                .iter()
                .filter(|(hash, c)| {
                    !c.pinned && !saved.draft.attachments.iter().any(|a| a.hash == hex(hash))
                })
                .min_by_key(|(_, c)| c.used)
                .map(|(h, c)| (*h, c.size));
            let Some((hash, size)) = next else {
                anyhow::bail!("缓存空间不足，保留内容不会自动清除")
            };
            let path = self.path.parent().unwrap().join("objects").join(hex(&hash));
            if path.exists() {
                std::fs::remove_dir_all(path)?;
            }
            saved.cached.remove(&hash);
            used -= size;
        }
        self.dirty = true;
        Ok(())
    }
    fn finish_blob(
        &mut self,
        hash: Digest,
        size: u64,
        pinned: bool,
        temp: tempfile::TempDir,
    ) -> Result<()> {
        if let Some(existing) = self.saved.as_mut().and_then(|s| s.cached.get_mut(&hash)) {
            existing.pinned |= pinned;
            existing.used = now();
            self.dirty = true;
            return Ok(());
        }
        let dest = self.blob_path(&hash);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        std::fs::rename(temp.path(), &dest)?;
        self.saved.as_mut().unwrap().cached.insert(
            hash,
            Cached {
                size,
                pinned,
                used: now(),
            },
        );
        self.dirty = true;
        Ok(())
    }
    pub(super) fn attach_file(&mut self, name: String, mut file: std::fs::File) -> Result<Value> {
        ensure!(
            self.allowed(&self.key.public().to_peer_id()),
            "当前成员没有附件权限"
        );
        ensure!(
            !name.is_empty() && name.len() <= 255 && !name.chars().any(char::is_control),
            "附件名无效"
        );
        let size = file.metadata()?.len();
        self.make_room(size)?;
        std::fs::create_dir_all(self.blobs())?;
        let temp = tempfile::Builder::new()
            .prefix("upload-")
            .tempdir_in(self.blobs())?;
        let mut digest = Sha256::new();
        let mut total = 0u64;
        let mut index = 0;
        loop {
            let mut bytes = vec![0; CHUNK];
            let mut n = 0;
            while n < CHUNK {
                let read = file.read(&mut bytes[n..])?;
                if read == 0 {
                    break;
                }
                n += read;
            }
            if n == 0 {
                break;
            }
            bytes.truncate(n);
            total += n as u64;
            ensure!(
                total <= MAX_ATTACHMENT_BYTES && total <= size,
                "附件在读取时变化或超限"
            );
            digest.update(&bytes);
            super::super::storage::save_bytes(
                &temp.path().join(index.to_string()),
                &self.storage_key,
                &bytes,
            )?;
            bytes.zeroize();
            index += 1;
        }
        ensure!(total == size, "附件读取不完整");
        let hash: Digest = digest.finalize().into();
        self.finish_blob(hash, size, false, temp)?;
        Ok(json!({"name":name,"size":size,"hash":hex(&hash)}))
    }
    pub(super) fn attachment_size(&self, hash: &Digest) -> Option<u64> {
        self.saved
            .as_ref()?
            .forum
            .records
            .iter()
            .filter(|(id, _)| self.shareable(id))
            .find_map(|(_, r)| match &r.record.action {
                Action::Content { attachments, .. } => {
                    attachments.iter().find(|a| &a.hash == hash).map(|a| a.size)
                }
                _ => None,
            })
    }
    pub(super) fn read_chunk(&self, hash: &Digest, offset: u64) -> Result<Option<Vec<u8>>> {
        ensure!(offset % (CHUNK as u64) == 0, "分块偏移无效");
        let Some(c) = self.saved.as_ref().and_then(|s| s.cached.get(hash)) else {
            return Ok(None);
        };
        ensure!(offset <= c.size, "分块超出附件");
        if offset == c.size {
            return Ok(Some(vec![]));
        }
        let path = self
            .blob_path(hash)
            .join((offset / CHUNK as u64).to_string());
        ensure!(
            std::fs::metadata(&path)?.len() <= CHUNK as u64 + 128,
            "缓存分块过大"
        );
        let bytes = super::super::storage::load_bytes(&path, &self.storage_key)?;
        ensure!(
            bytes.len() == (c.size - offset).min(CHUNK as u64) as usize,
            "缓存分块大小不符"
        );
        Ok(Some(bytes))
    }
    pub(super) fn export_file(&mut self, hash: Digest, mut file: std::fs::File) -> Result<()> {
        ensure!(
            self.allowed(&self.key.public().to_peer_id()),
            "当前成员没有附件权限"
        );
        let size = self
            .saved
            .as_ref()
            .and_then(|s| s.cached.get(&hash))
            .ok_or_else(|| anyhow::anyhow!("附件尚未下载完整"))?
            .size;
        ensure!(size <= MAX_ATTACHMENT_BYTES, "附件大小无效");
        let mut verified = zeroize::Zeroizing::new(Vec::with_capacity(size as usize));
        let mut digest = Sha256::new();
        let mut offset = 0;
        while offset < size {
            let mut bytes = self
                .read_chunk(&hash, offset)?
                .ok_or_else(|| anyhow::anyhow!("附件缓存缺失"))?;
            digest.update(&bytes);
            verified.extend_from_slice(&bytes);
            offset += bytes.len() as u64;
            bytes.zeroize();
        }
        ensure!(Digest::from(digest.finalize()) == hash, "缓存附件校验失败");
        file.write_all(&verified)?;
        file.sync_all()?;
        self.saved
            .as_mut()
            .unwrap()
            .cached
            .get_mut(&hash)
            .unwrap()
            .used = now();
        self.dirty = true;
        Ok(())
    }
    pub(super) fn preview(&mut self, hash: Digest) -> Result<Value> {
        ensure!(
            self.allowed(&self.key.public().to_peer_id()),
            "当前成员没有附件权限"
        );
        let c = self
            .saved
            .as_ref()
            .and_then(|s| s.cached.get(&hash))
            .ok_or_else(|| anyhow::anyhow!("图片尚未下载完整"))?;
        ensure!(c.size <= 5_000_000, "预览超过 5 MB，请保存文件查看");
        let mut bytes = Vec::with_capacity(c.size as usize);
        while bytes.len() < c.size as usize {
            let chunk = self
                .read_chunk(&hash, bytes.len() as u64)?
                .ok_or_else(|| anyhow::anyhow!("图片缓存缺失"))?;
            bytes.extend(chunk);
        }
        ensure!(content_id(&bytes) == hash, "图片校验失败");
        let encoded = URL_SAFE_NO_PAD.encode(&bytes);
        bytes.zeroize();
        Ok(json!({"bytes":encoded}))
    }
    pub(super) fn start_download(&mut self, hash: Digest) -> Result<()> {
        if self
            .saved
            .as_ref()
            .is_some_and(|s| s.cached.contains_key(&hash))
            || self.downloads.contains_key(&hash)
        {
            return Ok(());
        }
        let size = self
            .attachment_size(&hash)
            .ok_or_else(|| anyhow::anyhow!("索引中没有这个附件"))?;
        ensure!(self.downloads.len() < 2, "同时最多下载两个附件，请稍后重试");
        self.make_room(size)?;
        std::fs::create_dir_all(self.blobs())?;
        let temporary = tempfile::Builder::new()
            .prefix("receive-")
            .tempdir_in(self.blobs())?;
        self.downloads.insert(
            hash,
            Download {
                hash,
                size,
                received: 0,
                digest: Sha256::new(),
                temporary,
                updated: Instant::now(),
            },
        );
        for peer in self.peers.clone().into_iter().take(3) {
            self.enqueue(peer, Query::Chunk { hash, offset: 0 });
        }
        self.status = "正在查询附件副本".into();
        Ok(())
    }
    pub(super) fn receive_chunk(
        &mut self,
        peer: PeerId,
        hash: Digest,
        offset: u64,
        total: u64,
        bytes: Option<Vec<u8>>,
    ) -> Result<()> {
        ensure!(self.allowed(&peer), "对方成员资格失效");
        let Some(mut bytes) = bytes else {
            return Ok(());
        };
        let Some(d) = self.downloads.get_mut(&hash) else {
            return Ok(());
        };
        if offset != d.received {
            return Ok(());
        }
        ensure!(
            total == d.size && bytes.len() == (total - offset).min(CHUNK as u64) as usize,
            "附件分块长度不符"
        );
        ensure!(!bytes.is_empty() || total == 0, "空附件分块无效");
        if !bytes.is_empty() {
            super::super::storage::save_bytes(
                &d.temporary.path().join((offset / CHUNK as u64).to_string()),
                &self.storage_key,
                &bytes,
            )?;
        }
        d.digest.update(&bytes);
        d.received += bytes.len() as u64;
        d.updated = Instant::now();
        bytes.zeroize();
        if d.received == d.size {
            let d = self.downloads.remove(&hash).unwrap();
            ensure!(
                Digest::from(d.digest.finalize()) == d.hash,
                "附件哈希不匹配，请换副本重试"
            );
            self.finish_blob(hash, d.size, false, d.temporary)?;
            self.wanted.remove(&hash);
            self.status = "附件已校验并缓存，可以保存".into();
        } else {
            let offset = d.received;
            self.enqueue(peer, Query::Chunk { hash, offset });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corrupt_cached_attachment_never_reaches_export_target() {
        let dir = tempfile::tempdir().unwrap();
        let mut worker = Worker::new(
            Keypair::ed25519_from_bytes([1; 32]).unwrap(),
            [9; 32],
            dir.path().into(),
        )
        .unwrap();
        worker.saved = Some(Saved {
            name: "test".into(),
            forum: Forum::new([7; 32], worker.key.public().encode_protobuf()).unwrap(),
            cached: BTreeMap::new(),
            draft: DraftData::default(),
            entries: vec![],
        });
        worker.refresh();
        let source = dir.path().join("source");
        std::fs::write(&source, b"original").unwrap();
        let attachment = worker
            .attach_file("source".into(), std::fs::File::open(source).unwrap())
            .unwrap();
        let hash = parse_hash(attachment["hash"].as_str().unwrap()).unwrap();
        super::super::super::storage::save_bytes(
            &worker.blob_path(&hash).join("0"),
            &worker.storage_key,
            b"tampered",
        )
        .unwrap();
        let output = dir.path().join("output");
        std::fs::write(&output, b"keep").unwrap();
        assert!(worker
            .export_file(
                hash,
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&output)
                    .unwrap()
            )
            .is_err());
        assert_eq!(std::fs::read(output).unwrap(), b"keep");
    }
}
