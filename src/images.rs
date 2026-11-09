//! 仅处理显式发送的图片；私聊预览缓存不属于论坛对象存储。
use std::{
    fs,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{bail, ensure, Context, Result};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use fs2::FileExt;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use rand::RngCore;
use zeroize::Zeroizing;

pub const MAX_IMAGE_BYTES: usize = 5_000_000;
pub const MAX_IMAGE_PIXELS: u64 = 20_000_000;
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CACHE_ITEMS: usize = 128;
const MAX_BLOB_BYTES: usize = MAX_IMAGE_BYTES + 24 + 16;

pub struct ImageData {
    pub mime: String,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

fn decoder(bytes: &[u8]) -> Result<impl ImageDecoder + '_> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_IMAGE_BYTES,
        "图片为空或超过 5 MB"
    );
    let format = image::guess_format(bytes).context("无法识别图片格式")?;
    ensure!(
        matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
        ),
        "只支持 PNG、JPEG、WebP 图片"
    );
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_PIXELS as u32);
    limits.max_image_height = Some(MAX_IMAGE_PIXELS as u32);
    // image 的分配预算为尽力限制；像素与解码输出另行严格检查。
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let decoder = reader.into_decoder().context("图片头损坏或超限")?;
    let (width, height) = decoder.dimensions();
    ensure!(
        width > 0 && height > 0 && u64::from(width) * u64::from(height) <= MAX_IMAGE_PIXELS,
        "图片超过 2000 万像素"
    );
    ensure!(
        decoder.total_bytes() <= 128 * 1024 * 1024,
        "图片解码内存超限"
    );
    Ok(decoder)
}

struct BoundedOutput(Cursor<Vec<u8>>);

impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.position().saturating_add(bytes.len() as u64) > MAX_IMAGE_BYTES as u64 {
            return Err(std::io::Error::other("处理后的图片超过 5 MB"));
        }
        self.0.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl Seek for BoundedOutput {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.0.seek(position)
    }
}

pub fn validate_and_normalize(bytes: &[u8]) -> Result<ImageData> {
    let mut decoder = decoder(bytes)?;
    let orientation = decoder.orientation().context("图片方向信息损坏")?;
    let mut image = DynamicImage::from_decoder(decoder).context("图片解码失败")?;
    image.apply_orientation(orientation);
    // 从像素重新编码，不复制 EXIF、文本块或原文件中的附加数据。
    let (format, mime) = if image.color().has_alpha() {
        (ImageFormat::Png, "image/png")
    } else {
        (ImageFormat::Jpeg, "image/jpeg")
    };
    let (width, height) = (image.width(), image.height());
    let mut out = BoundedOutput(Cursor::new(Vec::new()));
    image
        .write_to(&mut out, format)
        .context("图片重新编码失败")?;
    let bytes = out.0.into_inner();
    ensure!(
        bytes.len() <= MAX_IMAGE_BYTES,
        "处理后的图片超过 5 MB，请缩小后重试"
    );
    Ok(ImageData {
        mime: mime.into(),
        bytes,
        width,
        height,
    })
}

/// 由原生层选择专用目录与每身份独立密钥；页面只持有随机 ID。
/// 满额时拒绝接收，由调用方决定清理或提示，不扫描用户下载目录。
pub struct ImageCache {
    dir: PathBuf,
    key: Zeroizing<[u8; 32]>,
    write_lock: Mutex<()>,
}

impl ImageCache {
    pub fn new(dir: impl AsRef<Path>, key: [u8; 32]) -> Result<Self> {
        fs::create_dir_all(dir.as_ref())?;
        Ok(Self {
            dir: dir.as_ref().to_owned(),
            key: Zeroizing::new(key),
            write_lock: Mutex::new(()),
        })
    }

    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let image = validate_and_normalize(bytes)?;
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("图片缓存锁异常"))?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(".write.lock"))?;
        FileExt::lock_exclusive(&lock)?;
        let mut count = 0;
        let mut used = 0u64;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.path().extension().is_some_and(|e| e == "image") {
                let metadata = entry.metadata()?;
                ensure!(metadata.is_file(), "图片缓存含异常条目");
                count += 1;
                used = used
                    .checked_add(metadata.len())
                    .context("图片缓存大小溢出")?;
            }
        }
        ensure!(
            count < MAX_CACHE_ITEMS && used + image.bytes.len() as u64 + 40 <= MAX_CACHE_BYTES,
            "图片缓存已满"
        );
        let mut random = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut random);
        let id: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let mut nonce = [0u8; 24];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let cipher = XChaCha20Poly1305::new_from_slice(self.key.as_ref()).expect("32B key");
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &image.bytes,
                    aad: id.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("图片缓存加密失败"))?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.dir)?;
        temporary.write_all(&nonce)?;
        temporary.write_all(&ciphertext)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(self.path(&id)?)
            .context("图片缓存保存失败")?;
        Ok(id)
    }

    pub fn get(&self, id: &str) -> Result<ImageData> {
        let path = self.path(id)?;
        ensure!(
            !fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "拒绝图片缓存符号链接"
        );
        let file = fs::File::open(path)?;
        let mut blob = Vec::new();
        file.take((MAX_BLOB_BYTES + 1) as u64)
            .read_to_end(&mut blob)?;
        ensure!(
            (40..=MAX_BLOB_BYTES).contains(&blob.len()),
            "图片缓存长度非法"
        );
        let cipher = XChaCha20Poly1305::new_from_slice(self.key.as_ref()).expect("32B key");
        let bytes = cipher
            .decrypt(
                XNonce::from_slice(&blob[..24]),
                Payload {
                    msg: &blob[24..],
                    aad: id.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("图片缓存校验失败"))?;
        let decoded = decoder(&bytes)?;
        let (width, height) = decoded.dimensions();
        drop(decoded);
        let mime = match image::guess_format(&bytes)? {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            _ => bail!("图片缓存格式非法"),
        };
        Ok(ImageData {
            mime: mime.into(),
            bytes,
            width,
            height,
        })
    }

    /// 只接受调用方的索引字节，不解析内容；固定文件名不对页面开放。
    pub fn save_index(&self, bytes: &[u8]) -> Result<()> {
        ensure!(bytes.len() <= 1_000_000, "图片索引超过 1 MB");
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("图片缓存锁异常"))?;
        let mut nonce = [0u8; 24];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let cipher = XChaCha20Poly1305::new_from_slice(self.key.as_ref()).expect("32B key");
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: bytes,
                    aad: b"spark-chat/image-index/v1",
                },
            )
            .map_err(|_| anyhow::anyhow!("图片索引加密失败"))?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.dir)?;
        temporary.write_all(&nonce)?;
        temporary.write_all(&ciphertext)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.dir.join("index.enc"))
            .context("图片索引保存失败")?;
        Ok(())
    }

    pub fn load_index(&self) -> Result<Option<Vec<u8>>> {
        let file = match fs::File::open(self.dir.join("index.enc")) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut blob = Vec::new();
        file.take(1_000_041).read_to_end(&mut blob)?;
        ensure!((40..=1_000_040).contains(&blob.len()), "图片索引长度非法");
        let cipher = XChaCha20Poly1305::new_from_slice(self.key.as_ref()).expect("32B key");
        let bytes = cipher
            .decrypt(
                XNonce::from_slice(&blob[..24]),
                Payload {
                    msg: &blob[24..],
                    aad: b"spark-chat/image-index/v1",
                },
            )
            .map_err(|_| anyhow::anyhow!("图片索引校验失败"))?;
        Ok(Some(bytes))
    }

    fn path(&self, id: &str) -> Result<PathBuf> {
        ensure!(
            id.len() == 32
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "图片标识非法"
        );
        Ok(self.dir.join(format!("{id}.image")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = DynamicImage::new_rgba8(width, height);
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn rejects_disguised_oversized_and_excessive_pixels() {
        assert!(validate_and_normalize(b"<svg><script>alert(1)</script></svg>").is_err());
        assert!(validate_and_normalize(&vec![0; MAX_IMAGE_BYTES + 1]).is_err());
        assert!(validate_and_normalize(&png(5000, 4001)).is_err());
        let mut malformed = png(1, 1);
        malformed.truncate(25);
        assert!(validate_and_normalize(&malformed).is_err());
    }

    #[test]
    fn normalizes_pixels_and_discards_trailing_data() {
        let mut source = png(3, 2);
        source.extend_from_slice(b"PRIVATE_METADATA");
        let output = validate_and_normalize(&source).unwrap();
        assert_eq!((output.width, output.height), (3, 2));
        assert_eq!(output.mime, "image/png");
        assert!(!output.bytes.windows(16).any(|b| b == b"PRIVATE_METADATA"));
        assert!(image::load_from_memory(&output.bytes).is_ok());
    }

    #[test]
    fn encrypted_cache_rejects_wrong_key_tampering_swaps_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ImageCache::new(dir.path(), [7; 32]).unwrap();
        let source = png(3, 2);
        let id = cache.put(&source).unwrap();
        assert_eq!(cache.get(&id).unwrap().width, 3);
        let blob = fs::read(cache.path(&id).unwrap()).unwrap();
        assert!(!blob.windows(8).any(|b| b == &source[..8]));
        assert!(ImageCache::new(dir.path(), [8; 32])
            .unwrap()
            .get(&id)
            .is_err());
        assert!(cache.get("../../vault.json").is_err());
        let other = "a".repeat(32);
        fs::write(cache.path(&other).unwrap(), &blob).unwrap();
        assert!(cache.get(&other).is_err());
        let mut corrupt = blob;
        corrupt[30] ^= 1;
        fs::write(cache.path(&id).unwrap(), corrupt).unwrap();
        assert!(cache.get(&id).is_err());
    }

    #[test]
    fn encrypted_index_roundtrip_and_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ImageCache::new(dir.path(), [7; 32]).unwrap();
        assert!(cache.load_index().unwrap().is_none());
        let content = br#"{"peer":"private-contact"}"#;
        cache.save_index(content).unwrap();
        assert_eq!(cache.load_index().unwrap().unwrap(), content);
        let blob = fs::read(dir.path().join("index.enc")).unwrap();
        assert!(!blob.windows(content.len()).any(|b| b == content));
        assert!(ImageCache::new(dir.path(), [8; 32])
            .unwrap()
            .load_index()
            .is_err());
        assert!(cache.save_index(&vec![0; 1_000_001]).is_err());
        assert_eq!(cache.load_index().unwrap().unwrap(), content);
        cache.save_index(b"new").unwrap();
        assert_eq!(cache.load_index().unwrap().unwrap(), b"new");
    }

    #[test]
    fn cache_quota_rejects_without_removing_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ImageCache::new(dir.path(), [7; 32]).unwrap();
        let existing = dir.path().join(format!("{}.image", "a".repeat(32)));
        let file = fs::File::create(&existing).unwrap();
        file.set_len(MAX_CACHE_BYTES).unwrap();
        assert!(cache.put(&png(1, 1)).is_err());
        assert_eq!(fs::metadata(existing).unwrap().len(), MAX_CACHE_BYTES);
    }
}
