//! 加密库（子智能体 A 实现区）—— 密钥金字塔 v2
//!
//! ═══ 设计规格（必须逐条实现，见 git log 教学注释） ═══
//!
//! 金字塔：
//! ```text
//! 口令 --Argon2id--> K1      keyfile(32字节随机文件,另存) --Argon2id--> K2
//!        └────── MK = K1 XOR K2 ──────┘   （内存持有，永不落盘）
//! MK 解锁「静态身份密钥」ed25519 seed（32B，以 AEAD(MK) 包裹落盘）
//! 身份私钥 × 对方静态公钥(X25519) --> ECDH --> HKDF --> 每会话 DEK
//! DEK + ChaCha20Poly1305 AEAD --> 加密每条消息记录（密文落盘）
//! ```
//!
//! 落盘文件（vault_dir 下）：
//! - vault.json     : {version, salt1, salt2, tag1, tag2, wrapped_seed, peer_registry}
//!                    tag1/tag2 = Argon2 各自派生的校验标签（用于分辨是口令错还是 keyfile 错）
//! - msgs-<会话指纹>.binl : 每行 = u32长度 + postcard(EncMsgRow)
//!   EncMsgRow { nonce:[u8;12], ct:Vec<u8>, ts:i64, own:bool }
//!   （加密内容 = 明文消息字符串；nonce 递增）
//! - keyfile 由 create() 生成到用户指定路径（vault_dir 之外！），32 随机字节 hex 写入
//!
//! 解锁失败必须区分三类错误（UI 要如实展示）：
//! WrongPassphrase / MissingKeyfile / CorruptedVault
//!
//! 安全要点：
//! - Argon2id 默认安全参数（m=19456KiB t=2 p=1），不同 salt 于 K1/K2
//! - K1 校验：tag1 = HKDF(K1, "tag1")；K2 同理——单独可验证，组合后才能解 seed
//! - ECDH 用 x25519_dalek：身份 ed25519 seed 先转换或另存 x25519 静态密钥？
//!   简化决定：vault 另生成一对 x25519 静态密钥与 ed25519 并存，二者都以 MK 包裹存储。
//!   ed25519 给 libp2p 身份（PeerId），x25519 给会话 ECDH。
//!   peer_registry 记录 {peer_id -> x25519_pub(hex)}，连接后由 net 层调用 register_peer 存入。
//!
//! ═══ 公开接口（签名不得更改） ═══

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use argon2::Argon2;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use hkdf::Hkdf;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret as XStaticSecret};

pub type Result<T> = std::result::Result<T, VaultError>;

#[derive(Debug)]
pub enum VaultError {
    WrongPassphrase,
    MissingKeyfile,
    CorruptedVault(String),
    Io(std::io::Error),
    Other(String),
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultError::WrongPassphrase => write!(f, "口令不对"),
            VaultError::MissingKeyfile => write!(f, "缺少钥匙文件（keyfile）"),
            VaultError::CorruptedVault(s) => write!(f, "加密库损坏: {s}"),
            VaultError::Io(e) => write!(f, "读写错误: {e}"),
            VaultError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for VaultError {}

impl From<std::io::Error> for VaultError {
    fn from(e: std::io::Error) -> Self {
        VaultError::Io(e)
    }
}

/// 已解锁的会话句柄（DEK 在内存）
///
/// 一个 Session 对应「我和某个 peer 的聊天记录加密上下文」：
/// - dek          ：ECDH+HKDF 派生的每会话数据密钥（双方一致）
/// - nonce_prefix ：本 Session 打开的随机 4 字节前缀（见 store_msg 的 nonce 说明）
/// - counter      ：会话内递增的消息计数（生成 nonce 用）
pub struct Session {
    peer_id: String,
    dek: [u8; 32],
    nonce_prefix: [u8; 4],
    counter: AtomicU64,
}

/// 一条已解密的消息记录
#[derive(Debug, Clone)]
pub struct StoredMsg {
    pub ts: i64,
    pub own: bool, // true=我发的
    pub text: String,
}

pub struct Vault {
    // 私有字段自定
    vault_dir: PathBuf,
    /// 主密钥 MK=K1⊕K2。只在内存：换口令（change_passphrase）时要换新 MK 重包裹，
    /// 所以长期驻留；生产级实现应配 zeroize 擦除，教学版从简。
    mk: Mutex<[u8; 32]>,
    /// ed25519 身份种子（给 libp2p PeerId / net 层签名用）
    ed_seed: [u8; 32],
    /// x25519 静态私钥（只做会话 ECDH，两者职责分开是刻意的：身份≠密钥交换）
    x_secret: XStaticSecret,
    x_pub: [u8; 32],
    peer_id: String,
    /// vault.json 的内存缓存。registry 变更 / 换口令都是「改缓存→整体重写」，
    /// 避免读改写竞态；用 Mutex 是因为所有公开方法只拿 &self（UI 可能多线程调用）。
    file: Mutex<VaultFile>,
}

// ═══════════ 落盘结构 ═══════════

/// vault.json 的完整内容。所有二进制字段一律 hex 字符串（人可读、diff 友好）。
#[derive(Serialize, Deserialize)]
struct VaultFile {
    version: u32,
    /// Argon2 salt（K1=口令因子用）
    salt1: String,
    /// Argon2 salt（K2=keyfile 因子用）—— K1/K2 必须不同 salt，
    /// 否则同一输入会派生出相同的两个因子，XOR 语义就退化了
    salt2: String,
    /// HKDF(K1,"tag1") 的校验标签：能独立验证口令对不对
    tag1: String,
    /// HKDF(K2,"tag2")：独立验证 keyfile 对不对
    tag2: String,
    /// AEAD(wrap_key(MK), ed25519 seed)，hex(nonce12||ct)
    wrapped_ed: String,
    /// AEAD(wrap_key(MK), x25519 静态私钥)，hex(nonce12||ct)
    wrapped_x: String,
    /// 我的 x25519 公钥（公钥不保密，明文存，解锁前 UI 也能展示指纹）
    x25519_pub: String,
    /// peer_id -> 对方 x25519 公钥 hex
    peer_registry: BTreeMap<String, String>,
}

/// msgs-<指纹>.binl 的一行（postcard 序列化后带 u32 BE 长度前缀落盘）。
/// 注意 ts/own 是明文元数据（规格如此）：看得到「几点发的」但看不到「发了什么」。
#[derive(Serialize, Deserialize)]
struct EncMsgRow {
    nonce: [u8; 12],
    ct: Vec<u8>,
    ts: i64,
    own: bool,
}

// ═══════════ 常量 ═══════════

const VAULT_FILE: &str = "vault.json";
/// 密钥金字塔 v2；读到别的版本号说明库文件不是本程序写的
const VERSION: u32 = 2;
/// 单行消息上限 4MiB：防损坏文件里出现天文数字长度把内存读爆
const MAX_ROW: usize = 4 * 1024 * 1024;

/// AEAD 关联数据（AAD）标签——给「被包裹的种子」用。
/// 两个 blob 用不同 AAD，攻击者就没法把 wrapped_ed 和 wrapped_x 对调位置。
const AAD_ED: &[u8] = b"spark-chat/ed25519-seed";
const AAD_X: &[u8] = b"spark-chat/x25519-static";
const INFO_WRAP: &[u8] = b"spark-chat/wrap";
const INFO_TAG1: &[u8] = b"spark-chat/tag1";
const INFO_TAG2: &[u8] = b"spark-chat/tag2";
const INFO_DEK: &[u8] = b"spark-chat/dek/v1";

impl Vault {
    /// 首次创建：生成身份密钥对（ed25519+x25519）、keyfile、vault.json。
    /// keyfile_path 是用户选择的另存位置（不在 vault_dir 内）。
    /// 返回 12D3KooW... 形式的 PeerId 字符串。
    pub fn create(passphrase: &str, keyfile_path: &Path, vault_dir: &Path) -> Result<(Vault, String)> {
        Self::create_with_writer(passphrase, vault_dir, |content| {
            if let Some(parent) = keyfile_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(keyfile_path, content)?;
            Ok(())
        })
    }

    /// Android 使用系统文档 URI；只替换文件读写，密钥格式与密码学流程保持一致。
    pub fn create_with_writer(
        passphrase: &str, vault_dir: &Path,
        write_keyfile: impl FnOnce(&str) -> Result<()>,
    ) -> Result<(Vault, String)> {
        // 已有库则拒绝覆盖——身份密钥一旦丢了就再也解不开旧消息，宁停勿滥
        fs::create_dir_all(vault_dir)?;
        if vault_dir.join(VAULT_FILE).exists() {
            return Err(VaultError::Other(format!(
                "{} 里已有加密库，请改用解锁",
                vault_dir.display()
            )));
        }

        // ── 生成所有随机材料 ──
        let salt1 = rand32();
        let salt2 = rand32();
        let ed_seed = rand32();
        let x_secret = XStaticSecret::from(rand32()); // From<[u8;32]> 内部会做 clamp
        let x_pub = XPublicKey::from(&x_secret).to_bytes();
        let keyfile_bytes = rand32();

        // keyfile：32 随机字节以 hex 写出（64 个可打印字符，方便用户拷进 U 盘）
        let keyfile_content = hex_encode(&keyfile_bytes);
        write_keyfile(&keyfile_content)?; // 保存成功才创建加密库，失败可安全重试。

        // ── 双因子派生 ──
        let k1 = argon2_kdf(passphrase.as_bytes(), &salt1)?;
        let k2 = argon2_kdf(keyfile_content.as_bytes(), &salt2)?;
        let tag1 = hkdf32(&k1, None, INFO_TAG1);
        let tag2 = hkdf32(&k2, None, INFO_TAG2);
        let mk = xor32(&k1, &k2);
        let wrap_key = hkdf32(&mk, None, INFO_WRAP); // MK 不直接当 AEAD key：过一遍 HKDF 做域分隔

        // ── 包裹身份密钥并落盘 ──
        let file = VaultFile {
            version: VERSION,
            salt1: hex_encode(&salt1),
            salt2: hex_encode(&salt2),
            tag1: hex_encode(&tag1),
            tag2: hex_encode(&tag2),
            wrapped_ed: hex_encode(&wrap_blob(&wrap_key, &ed_seed, AAD_ED)),
            wrapped_x: hex_encode(&wrap_blob(&wrap_key, &x_secret.to_bytes(), AAD_X)),
            x25519_pub: hex_encode(&x_pub),
            peer_registry: BTreeMap::new(),
        };
        let peer_id = peer_id_from_ed25519_seed(&ed_seed)?;
        persist(vault_dir, &file)?;

        let vault = Vault {
            vault_dir: vault_dir.to_path_buf(),
            mk: Mutex::new(mk),
            ed_seed,
            x_secret,
            x_pub,
            peer_id: peer_id.clone(),
            file: Mutex::new(file),
        };
        Ok((vault, peer_id))
    }

    /// 解锁：口令 + keyfile → 验 tag → 解出密钥驻内存。
    pub fn unlock(passphrase: &str, keyfile_path: &Path, vault_dir: &Path) -> Result<Vault> {
        Self::unlock_with_reader(passphrase, vault_dir, || read_keyfile(keyfile_path))
    }

    pub fn unlock_with_reader(
        passphrase: &str, vault_dir: &Path,
        read: impl FnOnce() -> Result<String>,
    ) -> Result<Vault> {
        // 1) 读 vault.json：文件缺失/JSON 解不开/版本不对 → 库本身坏了
        let raw = match fs::read_to_string(vault_dir.join(VAULT_FILE)) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(VaultError::CorruptedVault(format!(
                    "找不到 {}（目录里没有加密库）",
                    vault_dir.join(VAULT_FILE).display()
                )));
            }
            Err(e) => return Err(VaultError::Io(e)),
        };
        let file: VaultFile = serde_json::from_str(&raw)
            .map_err(|e| VaultError::CorruptedVault(format!("vault.json 解析失败: {e}")))?;
        if file.version != VERSION {
            return Err(VaultError::CorruptedVault(format!(
                "版本不匹配（文件 v{}，程序 v{VERSION}）",
                file.version
            )));
        }

        // 2) 读 keyfile：文件不在 → 第二因子丢失，这是与口令错误不同的错误
        let keyfile_content = read()?.trim().to_owned();

        // 3) 先验口令（tag1 只依赖 K1，不需要 keyfile 就能判断口令对错）
        let salt1 = file.salt1_bytes()?;
        let k1 = argon2_kdf(passphrase.as_bytes(), &salt1)?;
        if hkdf32(&k1, None, INFO_TAG1) != file.tag1_bytes()? {
            return Err(VaultError::WrongPassphrase);
        }

        // 4) 再验 keyfile（tag2 只依赖 K2）。keyfile 在但内容不对 = 拿错了钥匙文件，
        //    语义上仍归入 MissingKeyfile（三类错误里它最接近「第二因子不对」）
        let salt2 = file.salt2_bytes()?;
        let k2 = argon2_kdf(keyfile_content.as_bytes(), &salt2)?;
        if hkdf32(&k2, None, INFO_TAG2) != file.tag2_bytes()? {
            return Err(VaultError::MissingKeyfile);
        }

        // 5) MK 解包裹：tag 都过了但 AEAD 解不开 → 库被篡改/磁盘损坏
        let mk = xor32(&k1, &k2);
        let wrap_key = hkdf32(&mk, None, INFO_WRAP);
        let ed_seed = unwrap_blob(&wrap_key, &file.wrapped_ed_bytes()?, AAD_ED)?;
        let x_sec_bytes = unwrap_blob(&wrap_key, &file.wrapped_x_bytes()?, AAD_X)?;
        let x_secret = XStaticSecret::from(x_sec_bytes);
        let x_pub = XPublicKey::from(&x_secret).to_bytes();
        // 公钥字段是明文存的，重算一遍对比：防有人改了 vault.json 里的 x25519_pub
        if x_pub != file.x25519_pub_bytes()? {
            return Err(VaultError::CorruptedVault(
                "x25519 公钥字段与私钥对不上，库被篡改".into(),
            ));
        }

        // PeerId 从种子重算（不信任文件里没有的字段，也确实没存——幂等更安全）
        let peer_id = peer_id_from_ed25519_seed(&ed_seed)?;

        Ok(Vault {
            vault_dir: vault_dir.to_path_buf(),
            mk: Mutex::new(mk),
            ed_seed,
            x_secret,
            x_pub,
            peer_id,
            file: Mutex::new(file),
        })
    }

    /// 我的 PeerId（12D3KooW...）
    pub fn peer_id(&self) -> String {
        self.peer_id.clone()
    }

    /// ed25519 身份私钥 seed（libp2p 构造 identity 用；net 层负责转换）
    pub fn ed25519_seed(&self) -> [u8; 32] {
        self.ed_seed
    }

    /// 我的 x25519 静态公钥（发给对方用）
    pub fn x25519_pub(&self) -> [u8; 32] {
        self.x_pub
    }

    /// 记住对方（连接后调用）：peer_id -> 对方 x25519 公钥
    pub fn register_peer(&self, peer_id: &str, x25519_pub: &[u8; 32]) -> Result<()> {
        if peer_id.is_empty() {
            return Err(VaultError::Other("peer_id 不能为空".into()));
        }
        let mut file = self.lock_file()?;
        file.peer_registry
            .insert(peer_id.to_string(), hex_encode(x25519_pub));
        persist(&self.vault_dir, &file)
    }

    /// 打开（或创建）与某个 peer 的会话：ECDH(我的x25519私钥, 对方x25519公钥) -> HKDF -> DEK
    pub fn open_session(&self, peer_id: &str) -> Result<Session> {
        // 没登记过对方公钥就没法做 ECDH——net 层握手后应先 register_peer
        let their_hex = {
            let file = self.lock_file()?;
            file.peer_registry
                .get(peer_id)
                .cloned()
                .ok_or_else(|| VaultError::Other(format!("还没登记 {peer_id} 的 x25519 公钥")))?
        };
        let their_pub_bytes = hex32(&their_hex)?;

        // ── ECDH：x25519 静态-静态（Signal/Telegram 的 PreKey 简化版）──
        let shared = self.x_secret.diffie_hellman(&XPublicKey::from(their_pub_bytes));

        // HKDF 盐 = 双方公钥排序拼接。排序是为了「谁先谁后」不影响结果，
        // 我和他独立各自计算也得到同一个 DEK——否则 A→B 和 B→A 会派生出两把钥匙。
        let (lo, hi) = if self.x_pub <= their_pub_bytes {
            (self.x_pub, their_pub_bytes)
        } else {
            (their_pub_bytes, self.x_pub)
        };
        let mut salt = [0u8; 64];
        salt[..32].copy_from_slice(&lo);
        salt[32..].copy_from_slice(&hi);
        let dek = hkdf32(shared.as_bytes(), Some(&salt), INFO_DEK);

        // nonce 前缀随机化：见 store_msg 里的说明（防重启后计数器归零造成 nonce 重用）
        let mut nonce_prefix = [0u8; 4];
        rand::rngs::OsRng.fill_bytes(&mut nonce_prefix);

        Ok(Session {
            peer_id: peer_id.to_string(),
            dek,
            nonce_prefix,
            counter: AtomicU64::new(0),
        })
    }

    /// 追加一条消息（加密落盘）
    pub fn store_msg(&self, s: &Session, own: bool, ts: i64, text: &str) -> Result<()> {
        // nonce = [4B 随机会话前缀][8B 递增计数器]
        // 为什么不每次纯随机 96 位？规格要求会话内递增；为什么不能从 0 纯递增？
        // 因为 DEK 是静态 ECDH 派生的、跨重启不变，重启后计数器归零就会复用
        // (key,nonce) —— AEAD 下这是灾难性错误。加 4 字节随机前缀（WireGuard 同款
        // 思路）让每个 Session 的 nonce 空间互不重叠，计数器本会话内严格递增。
        let n = s.counter.fetch_add(1, Ordering::Relaxed);
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&s.nonce_prefix);
        nonce[4..].copy_from_slice(&n.to_be_bytes());

        // AAD 绑定明文元数据 (ts,own)：行内 ts/own 虽是明文，但被改动后解密必失败，
        // 时间戳不可伪造——「能看到几点发」和「能改几点发」是两回事。
        let aad = msg_aad(ts, own);
        let ct = seal(&s.dek, &nonce, &aad, text.as_bytes());

        let row = EncMsgRow { nonce, ct, ts, own };
        let bytes =
            postcard::to_allocvec(&row).map_err(|e| VaultError::Other(format!("序列化失败: {e}")))?;

        // 追加写：聊天记录只增不改，O(1) 落盘
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.msg_path(&s.peer_id))?;
        f.write_all(&(bytes.len() as u32).to_be_bytes())?;
        f.write_all(&bytes)?;
        Ok(())
    }

    /// 读出全部消息（解密到内存）
    pub fn load_msgs(&self, s: &Session) -> Result<Vec<StoredMsg>> {
        let path = self.msg_path(&s.peer_id);
        let data = match fs::read(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()), // 新会话没历史
            Err(e) => return Err(VaultError::Io(e)),
        };

        let mut out = Vec::new();
        let mut off = 0usize;
        while off < data.len() {
            // 任何一步走不下都说明文件被截断/篡改 → 如实报 CorruptedVault，不静默吞
            if data.len() - off < 4 {
                return Err(VaultError::CorruptedVault(format!(
                    "{} 末尾有残缺的长度前缀",
                    path.display()
                )));
            }
            let n = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
            if n == 0 || n > MAX_ROW {
                return Err(VaultError::CorruptedVault(format!(
                    "{} 第 {} 条长度非法（{n}B）",
                    path.display(),
                    out.len()
                )));
            }
            off += 4;
            if data.len() - off < n {
                return Err(VaultError::CorruptedVault(format!(
                    "{} 第 {}条被截断",
                    path.display(),
                    out.len()
                )));
            }
            let row: EncMsgRow = postcard::from_bytes(&data[off..off + n])
                .map_err(|e| VaultError::CorruptedVault(format!("第 {} 条反序列化失败: {e}", out.len())))?;
            off += n;

            let pt = open_seal(&s.dek, &row.nonce, &msg_aad(row.ts, row.own), &row.ct)?;
            let text = String::from_utf8(pt).map_err(|_| {
                VaultError::CorruptedVault(format!("第 {} 条解出的不是 UTF-8 文本", out.len()))
            })?;
            out.push(StoredMsg { ts: row.ts, own: row.own, text });
        }
        Ok(out)
    }

    /// 导出明文（唯一明文出口，UI 弹提醒后调用）
    pub fn export_plain(&self, s: &Session, out_path: &Path) -> Result<usize> {
        let msgs = self.load_msgs(s)?;
        let mut f = fs::File::create(out_path)?;
        for m in &msgs {
            let who = if m.own { "我" } else { "对方" };
            writeln!(f, "[{}] {who}: {}", m.ts, m.text)?;
        }
        f.flush()?;
        Ok(msgs.len())
    }

    /// 修改口令（保留 keyfile；重包裹 MK）
    pub fn change_passphrase(&self, old: &str, new: &str, keyfile_path: &Path) -> Result<()> {
        let keyfile_content = read_keyfile(keyfile_path)?;
        let mut file = self.lock_file()?;

        // 验旧口令 + 验 keyfile（流程同 unlock，双因子缺一不可改口令）
        let salt1 = file.salt1_bytes()?;
        let salt2 = file.salt2_bytes()?;
        let k1_old = argon2_kdf(old.as_bytes(), &salt1)?;
        if hkdf32(&k1_old, None, INFO_TAG1) != file.tag1_bytes()? {
            return Err(VaultError::WrongPassphrase);
        }
        let k2 = argon2_kdf(keyfile_content.as_bytes(), &salt2)?;
        if hkdf32(&k2, None, INFO_TAG2) != file.tag2_bytes()? {
            return Err(VaultError::MissingKeyfile);
        }

        // 自检：当前内存里的 MK 确实还能解开库里的旧包裹。
        // 防止「vault.json 在运行中被别的程序换掉」这种极端情况把错误的东西重包裹进去。
        let mk_old = *self
            .mk
            .lock()
            .map_err(|_| VaultError::Other("内部锁中毒".into()))?;
        let wrap_old = hkdf32(&mk_old, None, INFO_WRAP);
        unwrap_blob(&wrap_old, &file.wrapped_ed_bytes()?, AAD_ED)?;

        // 新口令用全新 salt 重新派生 K1（旧 K1 与新 K1 无任何关系）。
        // 注意 MK 变了但身份密钥本身不变——历史消息的 DEK 不受影响，换口令后照常可读。
        let salt1_new = rand32();
        let k1_new = argon2_kdf(new.as_bytes(), &salt1_new)?;
        let mk_new = xor32(&k1_new, &k2);
        let wrap_new = hkdf32(&mk_new, None, INFO_WRAP);

        file.salt1 = hex_encode(&salt1_new);
        file.tag1 = hex_encode(&hkdf32(&k1_new, None, INFO_TAG1));
        file.wrapped_ed = hex_encode(&wrap_blob(&wrap_new, &self.ed_seed, AAD_ED));
        file.wrapped_x = hex_encode(&wrap_blob(&wrap_new, &self.x_secret.to_bytes(), AAD_X));
        persist(&self.vault_dir, &file)?;

        // 内存状态同步成新 MK（密钥本体没换，只是换了包裹它的锁）
        *self
            .mk
            .lock()
            .map_err(|_| VaultError::Other("内部锁中毒".into()))? = mk_new;
        Ok(())
    }

    // ── 内部工具 ──

    fn lock_file(&self) -> Result<MutexGuard<'_, VaultFile>> {
        self.file
            .lock()
            .map_err(|_| VaultError::Other("内部锁中毒".into()))
    }

    /// 会话指纹 = SHA-256(peer_id) 前 16 个 hex 字符。
    /// 用哈希而不是 peer_id 本身：PeerId 是合法文件名，但定长指纹让文件名
    /// 不会随 PeerId 类型变化，也让目录列表不直接暴露完整 PeerId。
    fn msg_path(&self, peer_id: &str) -> PathBuf {
        let fp = hex_encode(&Sha256::digest(peer_id.as_bytes()));
        self.vault_dir.join(format!("msgs-{}.binl", &fp[..16]))
    }
}

impl VaultFile {
    fn salt1_bytes(&self) -> Result<[u8; 32]> {
        hex32(&self.salt1)
    }
    fn salt2_bytes(&self) -> Result<[u8; 32]> {
        hex32(&self.salt2)
    }
    fn tag1_bytes(&self) -> Result<[u8; 32]> {
        hex32(&self.tag1)
    }
    fn tag2_bytes(&self) -> Result<[u8; 32]> {
        hex32(&self.tag2)
    }
    fn wrapped_ed_bytes(&self) -> Result<Vec<u8>> {
        hex_decode(&self.wrapped_ed)
    }
    fn wrapped_x_bytes(&self) -> Result<Vec<u8>> {
        hex_decode(&self.wrapped_x)
    }
    fn x25519_pub_bytes(&self) -> Result<[u8; 32]> {
        hex32(&self.x25519_pub)
    }
}

// ═══════════ 密码学小工具 ═══════════

/// Argon2id，crate 默认参数（m=19456KiB, t=2, p=1, 输出 32B）——规格指定用默认。
fn argon2_kdf(input: &[u8], salt: &[u8]) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    Argon2::default()
        .hash_password_into(input, salt, &mut out)
        .map_err(|e| VaultError::Other(format!("Argon2id 失败: {e}")))?;
    Ok(out)
}

/// HKDF-SHA256 → 32 字节。salt=None 时即「不加盐」（标签派生场景足够，info 已做域分隔）。
fn hkdf32(ikm: &[u8], salt: Option<&[u8]>, info: &[u8]) -> [u8; 32] {
    let mut okm = [0u8; 32];
    Hkdf::<Sha256>::new(salt, ikm)
        .expand(info, &mut okm)
        .expect("HKDF 输出 32B 恒在长度上限内");
    okm
}

fn xor32(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    // MK=K1⊕K2 的妙处：两因子任一不对都得到随机 MK，AEAD 解包裹必失败，
    // 且全程不需要把 K1/K2 单独存盘
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = a[i] ^ b[i];
    }
    out
}

fn rand32() -> [u8; 32] {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b
}

/// ChaCha20Poly1305 加密（内存操作，理论上不失败；expect 即可）
fn seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], pt: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new_from_slice(key).expect("AEAD key 恒为 32B");
    cipher
        .encrypt(Nonce::from_slice(nonce), Payload { msg: pt, aad })
        .expect("AEAD 加密不会失败")
}

/// 解密失败只可能是密文/标签被动过，统一映射为 CorruptedVault
fn open_seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new_from_slice(key).expect("AEAD key 恒为 32B");
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| VaultError::CorruptedVault("AEAD 校验失败（数据被篡改或密钥不对）".into()))
}

/// 包裹 32B 秘密：随机 nonce + AEAD → nonce(12)||ct(48)。每次包裹都换新 nonce。
fn wrap_blob(wrap_key: &[u8; 32], secret: &[u8; 32], aad: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let mut blob = nonce.to_vec();
    blob.extend_from_slice(&seal(wrap_key, &nonce, aad, secret));
    blob
}

/// 解包裹：切出 nonce 与 ct，AEAD 打开 → 32B
fn unwrap_blob(wrap_key: &[u8; 32], blob: &[u8], aad: &[u8]) -> Result<[u8; 32]> {
    if blob.len() != 12 + 32 + 16 {
        return Err(VaultError::CorruptedVault(format!(
            "包裹长度不对（{}B，应为 60B）",
            blob.len()
        )));
    }
    let pt = open_seal(wrap_key, &blob[..12].try_into().unwrap(), aad, &blob[12..])?;
    pt.try_into()
        .map_err(|_| VaultError::CorruptedVault("包裹内容不是 32B".into()))
}

/// 消息行 AAD：ts(8B BE) + own(1B)——见 store_msg 注释
fn msg_aad(ts: i64, own: bool) -> [u8; 9] {
    let mut a = [0u8; 9];
    a[..8].copy_from_slice(&ts.to_be_bytes());
    a[8] = own as u8;
    a
}

/// 从 ed25519 种子算 libp2p PeerId（12D3KooW... 形式，base58 的 identity multihash）
fn peer_id_from_ed25519_seed(seed: &[u8; 32]) -> Result<String> {
    // try_from_bytes 收 AsMut 且成功后会把缓冲 zeroize，所以传副本、留原件给 Vault
    let mut buf = *seed;
    let sk = libp2p_identity::ed25519::SecretKey::try_from_bytes(&mut buf)
        .map_err(|e| VaultError::Other(format!("ed25519 种子非法: {e}")))?;
    let kp = libp2p_identity::Keypair::from(libp2p_identity::ed25519::Keypair::from(sk));
    Ok(libp2p_identity::PeerId::from_public_key(&kp.public()).to_string())
}

// ═══════════ 编码与落盘工具 ═══════════

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(VaultError::CorruptedVault("hex 字段格式非法".into()));
    }
    (0..s.len() / 2)
        .map(|i| {
            u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|e| VaultError::CorruptedVault(format!("hex 解码失败: {e}")))
        })
        .collect()
}

fn hex32(s: &str) -> Result<[u8; 32]> {
    let v = hex_decode(s)?;
    v.try_into()
        .map_err(|_| VaultError::CorruptedVault("字段应为 32 字节（64 个 hex 字符）".into()))
}

/// 读 keyfile 并区分错误：文件不存在 = 第二因子丢失（MissingKeyfile），有别于内容错误
fn read_keyfile(p: &Path) -> Result<String> {
    match fs::read_to_string(p) {
        Ok(s) => Ok(s.trim().to_string()), // 容忍用户手工复制时带上的换行/空白
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(VaultError::MissingKeyfile),
        Err(e) => Err(VaultError::Io(e)),
    }
}

/// 原子写 vault.json：先写临时文件再 rename，崩溃时最多留下 .tmp，不会留下半个库文件
fn persist(vault_dir: &Path, file: &VaultFile) -> Result<()> {
    let json = serde_json::to_string_pretty(file)
        .map_err(|e| VaultError::Other(format!("序列化 vault.json 失败: {e}")))?;
    let tmp = vault_dir.join(format!("{VAULT_FILE}.tmp"));
    fs::write(&tmp, json)?;
    fs::rename(&tmp, vault_dir.join(VAULT_FILE))?;
    Ok(())
}

// ═══════════════════════ 单元测试 ═══════════════════════
//
// 覆盖：四类解锁结果（正例/错口令/缺 keyfile/库损坏）、消息往返、
// 密文落盘验证（明文绝不上盘）、换口令后旧消息仍可读、明文导出。

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 临时目录里放 vault/（库）与 keyfile.txt（第二因子，刻意放在库目录之外）
    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let vault_dir = tmp.path().join("vault");
        let keyfile = tmp.path().join("keyfile.txt");
        (tmp, vault_dir, keyfile)
    }

    /// 造一个「对方」：随机的 x25519 公钥 + 假 PeerId 字符串
    fn fake_peer() -> ([u8; 32], String) {
        let sk = XStaticSecret::from(rand32());
        (XPublicKey::from(&sk).to_bytes(), "12D3KooQFakePeerForTest0123456789".to_string())
    }

    fn unlocked(pass: &str, keyfile: &Path, vault_dir: &Path) -> Vault {
        Vault::unlock(pass, keyfile, vault_dir).unwrap()
    }

    #[test]
    fn document_keyfile_roundtrip_and_failed_export() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("vault");
        let failed = Vault::create_with_writer("test-pass", &dir, |_| {
            Err(VaultError::Io(std::io::Error::other("document write failed")))
        });
        assert!(failed.is_err());
        assert!(!dir.join(VAULT_FILE).exists());
        let mut document = String::new();
        let (created, _) = Vault::create_with_writer("test-pass", &dir, |text| {
            document = text.to_owned(); Ok(())
        }).unwrap();
        let opened = Vault::unlock_with_reader("test-pass", &dir, || Ok(format!(" {document}\n"))).unwrap();
        assert_eq!(created.peer_id(), opened.peer_id());
        let disk = temp.path().join("backup");
        fs::write(&disk, &document).unwrap();
        assert_eq!(Vault::unlock("test-pass", &disk, &dir).unwrap().peer_id(), created.peer_id());
        let mut wrote = false;
        assert!(Vault::create_with_writer("other", &dir, |_| { wrote=true; Ok(()) }).is_err());
        assert!(!wrote, "已有库时不得覆盖文档中的钥匙");
    }

    #[test]
    fn unlock_with_right_credentials_ok() {
        let (_tmp, vault_dir, keyfile) = setup();
        let (vault, peer_id) = Vault::create("correct horse", &keyfile, &vault_dir).unwrap();
        drop(vault);
        let again = unlocked("correct horse", &keyfile, &vault_dir);
        // PeerId 形如 12D3KooW...（ed25519 公钥的 identity multihash，base58 编码）
        assert!(peer_id.starts_with("12D3KooW"), "PeerId 形态不对: {peer_id}");
        assert_eq!(again.peer_id(), peer_id, "同一种子必须得到同一 PeerId");
    }

    #[test]
    fn unlock_wrong_passphrase() {
        let (_tmp, vault_dir, keyfile) = setup();
        Vault::create("right", &keyfile, &vault_dir).unwrap();
        assert!(matches!(
            Vault::unlock("wrong", &keyfile, &vault_dir),
            Err(VaultError::WrongPassphrase)
        ));
    }

    #[test]
    fn unlock_missing_keyfile() {
        let (tmp, vault_dir, keyfile) = setup();
        Vault::create("right", &keyfile, &vault_dir).unwrap();
        fs::remove_file(&keyfile).unwrap(); // U 盘忘插的场景
        assert!(matches!(
            Vault::unlock("right", &keyfile, &vault_dir),
            Err(VaultError::MissingKeyfile)
        ));
        assert!(tmp.path().exists(), "只是缺 keyfile，库目录不该被动");
    }

    #[test]
    fn unlock_corrupted_vault_json() {
        let (_tmp, vault_dir, keyfile) = setup();
        Vault::create("right", &keyfile, &vault_dir).unwrap();
        // 磁盘坏道/误编辑：把 JSON 拦腰截断
        let p = vault_dir.join(VAULT_FILE);
        let raw = fs::read_to_string(&p).unwrap();
        fs::write(&p, &raw[..raw.len() / 2]).unwrap();
        assert!(matches!(
            Vault::unlock("right", &keyfile, &vault_dir),
            Err(VaultError::CorruptedVault(_))
        ));
    }

    #[test]
    fn unlock_tampered_wrapped_key() {
        // vault.json 结构完好、口令和 keyfile 都对，但包裹密文被改了一个字节：
        // tag 校验过、AEAD 解不开 → CorruptedVault（不是口令错！）
        let (_tmp, vault_dir, keyfile) = setup();
        Vault::create("right", &keyfile, &vault_dir).unwrap();
        let p = vault_dir.join(VAULT_FILE);
        let mut v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        let s = v["wrapped_x"].as_str().unwrap().to_string();
        // 在 hex 值中间翻转一个字符（保持是合法 hex，专打 AEAD 校验）
        let mut chars: Vec<char> = s.chars().collect();
        let i = 20;
        chars[i] = if chars[i] == 'a' { 'b' } else { 'a' };
        v["wrapped_x"] = serde_json::Value::String(chars.into_iter().collect());
        fs::write(&p, serde_json::to_string(&v).unwrap()).unwrap();

        assert!(matches!(
            Vault::unlock("right", &keyfile, &vault_dir),
            Err(VaultError::CorruptedVault(_))
        ));
    }

    #[test]
    fn unlock_wrong_keyfile_content() {
        // keyfile 文件在、但内容是别的随机值：第二因子不对 → MissingKeyfile 桶
        let (_tmp, vault_dir, keyfile) = setup();
        Vault::create("right", &keyfile, &vault_dir).unwrap();
        fs::write(&keyfile, hex_encode(&rand32())).unwrap();
        assert!(matches!(
            Vault::unlock("right", &keyfile, &vault_dir),
            Err(VaultError::MissingKeyfile)
        ));
    }

    #[test]
    fn store_and_load_roundtrip() {
        let (_tmp, vault_dir, keyfile) = setup();
        let (vault, _) = Vault::create("pw", &keyfile, &vault_dir).unwrap();
        let (peer_pub, peer_id) = fake_peer();
        vault.register_peer(&peer_id, &peer_pub).unwrap();

        let s = vault.open_session(&peer_id).unwrap();
        vault.store_msg(&s, false, 1000, "在吗？").unwrap();
        vault.store_msg(&s, true, 2000, "在，啥事").unwrap();
        vault.store_msg(&s, true, 3000, "顺便发你个文件").unwrap();

        // 故意重新 open_session：新 Session 的 nonce 前缀不同，
        // 证明解密用的是每行存的 nonce，而不是会话内存状态
        let s2 = vault.open_session(&peer_id).unwrap();
        let msgs = vault.load_msgs(&s2).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].text, "在吗？");
        assert_eq!((msgs[0].ts, msgs[0].own), (1000, false));
        assert_eq!((msgs[2].ts, msgs[2].own), (3000, true));
    }

    #[test]
    fn plaintext_never_touches_disk() {
        let (tmp, vault_dir, keyfile) = setup();
        let (vault, _) = Vault::create("pw", &keyfile, &vault_dir).unwrap();
        let (peer_pub, peer_id) = fake_peer();
        vault.register_peer(&peer_id, &peer_pub).unwrap();
        let s = vault.open_session(&peer_id).unwrap();
        vault.store_msg(&s, false, 111, "考试答案：第三章 42，别外传").unwrap();
        vault.store_msg(&s, true, 222, "收到，看完就删").unwrap();

        // 直接读 msgs-*.binl 的原始字节，确认明文不在里面
        let binl: Vec<_> = fs::read_dir(&vault_dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "binl"))
            .collect();
        assert_eq!(binl.len(), 1, "应有且仅有一个会话消息文件");
        let bytes = fs::read(&binl[0]).unwrap();
        assert!(!bytes.is_empty());
        for secret in ["考试答案".as_bytes(), "别外传".as_bytes(), "看完就删".as_bytes()] {
            assert!(
                !bytes.windows(secret.len()).any(|w| w == secret),
                "明文泄漏进了 {}",
                binl[0].display()
            );
        }
        // 顺带扫 vault.json 和 keyfile（keyfile 在 vault 外但同在 tmp 下）
        for p in [vault_dir.join(VAULT_FILE), keyfile.clone()] {
            let b = fs::read(&p).unwrap();
            assert!(!b.windows(12).any(|w| w == "考试答案".as_bytes()));
        }
        // 但解密路径仍能完整读回（密文验证的另一面）
        let msgs = vault.load_msgs(&vault.open_session(&peer_id).unwrap()).unwrap();
        assert_eq!(msgs[0].text, "考试答案：第三章 42，别外传");
        let _ = tmp; // 仅保活临时目录
    }

    #[test]
    fn change_passphrase_keeps_history() {
        let (_tmp, vault_dir, keyfile) = setup();
        let (vault, _) = Vault::create("old-pw", &keyfile, &vault_dir).unwrap();
        let (peer_pub, peer_id) = fake_peer();
        vault.register_peer(&peer_id, &peer_pub).unwrap();
        vault
            .store_msg(&vault.open_session(&peer_id).unwrap(), true, 1, "历史消息")
            .unwrap();

        // 旧口令错 → 拒绝改
        assert!(matches!(
            vault.change_passphrase("nope", "new-pw", &keyfile),
            Err(VaultError::WrongPassphrase)
        ));
        vault.change_passphrase("old-pw", "new-pw", &keyfile).unwrap();

        // 旧口令解不开了，新口令可以
        assert!(matches!(
            Vault::unlock("old-pw", &keyfile, &vault_dir),
            Err(VaultError::WrongPassphrase)
        ));
        let v2 = unlocked("new-pw", &keyfile, &vault_dir);
        // 换口令只换「包裹」，身份密钥不变 → DEK 不变 → 历史照常可读
        let msgs = v2.load_msgs(&v2.open_session(&peer_id).unwrap()).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "历史消息");
    }

    #[test]
    fn export_plain_writes_file() {
        let (tmp, vault_dir, keyfile) = setup();
        let (vault, _) = Vault::create("pw", &keyfile, &vault_dir).unwrap();
        let (peer_pub, peer_id) = fake_peer();
        vault.register_peer(&peer_id, &peer_pub).unwrap();
        let s = vault.open_session(&peer_id).unwrap();
        vault.store_msg(&s, false, 7, "第一句").unwrap();
        vault.store_msg(&s, true, 8, "第二句").unwrap();

        let out = tmp.path().join("export.txt");
        let n = vault.export_plain(&s, &out).unwrap();
        assert_eq!(n, 2);
        let text = fs::read_to_string(&out).unwrap();
        assert!(text.contains("第一句") && text.contains("第二句"));
    }

    #[test]
    fn create_refuses_to_overwrite_existing_vault() {
        let (_tmp, vault_dir, keyfile) = setup();
        Vault::create("a", &keyfile, &vault_dir).unwrap();
        assert!(matches!(
            Vault::create("b", &keyfile, &vault_dir),
            Err(VaultError::Other(_))
        ));
    }

    #[test]
    fn session_requires_registered_peer() {
        let (_tmp, vault_dir, keyfile) = setup();
        let (vault, _) = Vault::create("pw", &keyfile, &vault_dir).unwrap();
        assert!(matches!(
            vault.open_session("12D3KooQNobodyRegisteredThis"),
            Err(VaultError::Other(_))
        ));
    }

    #[test]
    fn both_sides_derive_the_same_dek() {
        // 两个独立创建的库（各自的静态密钥、口令、目录），互登记公钥后
        // 必须各自派生出同一个 DEK——HKDF 盐用「双方公钥排序拼接」的功劳。
        // 这是端到端加密成立的前提，单独一侧的 roundtrip 测不出来。
        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        let (a, _) = Vault::create("pa", &tmp_a.path().join("kf"), &tmp_a.path().join("v")).unwrap();
        let (b, _) = Vault::create("pb", &tmp_b.path().join("kf"), &tmp_b.path().join("v")).unwrap();
        a.register_peer(&b.peer_id(), &b.x25519_pub()).unwrap();
        b.register_peer(&a.peer_id(), &a.x25519_pub()).unwrap();

        let sa = a.open_session(&b.peer_id()).unwrap();
        let sb = b.open_session(&a.peer_id()).unwrap();
        a.store_msg(&sa, true, 5, "alice发给bob").unwrap();

        // 模拟网络传输：alice 落盘的密文行原样搬到 bob 的库目录
        // （真实场景由 net 层搬运字节；这里只关心 bob 的 DEK 能否解开）
        fs::copy(a.msg_path(&b.peer_id()), b.msg_path(&a.peer_id())).unwrap();
        let got = b.load_msgs(&sb).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "alice发给bob");
        assert_eq!(got[0].ts, 5);

        // 反向：bob 加密的行 alice 也能解
        b.store_msg(&sb, false, 6, "bob回给alice").unwrap();
        fs::copy(b.msg_path(&a.peer_id()), a.msg_path(&b.peer_id())).unwrap();
        let got2 = a.load_msgs(&sa).unwrap();
        assert_eq!(got2.len(), 2);
        assert_eq!(got2[1].text, "bob回给alice");
    }
}
