//! 共享契约：线上帧格式与连接口令码。
//! 本文件是各模块（vault/net/ui）的唯一共享类型源，签名不得擅自更改。

use serde::{Deserialize, Serialize};

/// 线上传输的每一帧。postcard 序列化 + u32 BE 长度前缀。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Frame {
    /// 文本消息
    Text {
        /// 客户端生成的单调递增序号（对端回执引用）
        seq: u64,
        /// 发送方本地时间戳（毫秒）
        ts: i64,
        /// 明文内容（整条连接已被 Noise 加密，此处无需再加密）
        text: String,
    },
    /// 文件传输开始：先发元数据
    FileMeta {
        id: u64,
        name: String,
        size: u64,
        /// 整个文件的 SHA-256（hex）
        sha256: String,
    },
    /// 文件数据块（固定 256KiB，最后一块可小）
    FileChunk { id: u64, seq: u64, data: Vec<u8> },
    /// 文件传输结束/确认：接收方校验通过后回发
    FileDone { id: u64, sha256: String },
    /// 文件校验失败，请求重传
    FileReject { id: u64, reason: String },
}

/// 文件分块大小：256 KiB
pub const CHUNK_SIZE: usize = 256 * 1024;

/// 连接口令码（QQ/当面传输的那段短文本）。
/// 结构：SPARK-<base64url(精简连接信息)>，保留字母大小写。
///
/// 内含：我的监听地址列表 + 我的 PeerId + 4 字节随机 salt（用于双方显示相同的
/// "核对码"，电话核对防中间人——Telegram 秘密聊天的 emoji 指纹同款仪式）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectInfo {
    /// 形如 ["/ip4/10.7.15.63/tcp/41300", "/ip4/192.168.31.74/tcp/41300"]
    pub addrs: Vec<String>,
    /// 我的 PeerId 字符串
    pub peer_id: String,
    /// 随机 4 字节：双方各自显示其 hex 前 6 位作为「核对码」
    pub salt: [u8; 4],
}

// ── 连接码编解码（供 net 模块使用；UI 只显示字符串） ──
// base64url 本身含 '-'，不能把它和额外分隔符混在一起后统一删除。
// base64url 字母表（A-Za-z0-9-_）经 QQ/微信复制粘贴安全。

/// 生成形如 `SPARK-Ab3dEfg…` 的连接码
pub fn encode_conn_code(info: &ConnectInfo) -> anyhow::Result<String> {
    use base64::Engine;
    let bytes = postcard::to_allocvec(info)?;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
    Ok(format!("SPARK-{b64}"))
}

/// 解析连接码；兼容旧版每 12 个字符插入一个短横线的分组格式。
pub fn decode_conn_code(code: &str) -> anyhow::Result<ConnectInfo> {
    use base64::Engine;
    anyhow::ensure!(code.len() <= 8192, "连接码过长，请重新复制");
    let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    let body = compact
        .strip_prefix("SPARK-")
        .or_else(|| compact.strip_prefix("spark-"))
        .unwrap_or(&compact);
    let parse = |text: &str| -> Option<ConnectInfo> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(text)
            .ok()?;
        postcard::from_bytes(&bytes).ok()
    };
    if let Some(info) = parse(body) {
        return Ok(info);
    }
    let mut legacy = String::with_capacity(body.len());
    for (index, byte) in body.bytes().enumerate() {
        if (index + 1) % 13 == 0 {
            anyhow::ensure!(byte == b'-', "连接码格式不对，请整段复制重试");
        } else {
            anyhow::ensure!(
                byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_',
                "连接码格式不对，请整段复制重试"
            );
            legacy.push(byte as char);
        }
    }
    parse(&legacy).ok_or_else(|| anyhow::anyhow!("连接码格式不对，请整段复制重试"))
}

/// 由 salt 派生双方一致的「核对码」（hex 前 6 位，如 a3f0c2）
pub fn verify_code(salt: &[u8; 4]) -> String {
    salt.iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()[..6]
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conn_code_roundtrip() {
        let info = ConnectInfo {
            addrs: vec![
                "/ip4/10.7.15.63/tcp/41300".into(),
                "/ip4/192.168.31.74/tcp/41300".into(),
            ],
            peer_id: "12D3KooWLmJWTDQ9XDFQQtL1yqKwttPxJY1ykWW5xT6L4xwC5hgJ".into(),
            salt: [0xa3, 0xf0, 0xc2, 0x9b],
        };
        let code = encode_conn_code(&info).unwrap();
        assert!(code.starts_with("SPARK-"));
        assert!(code.len() < 200, "连接码应足够短可复制: {code}");
        // 模拟 QQ 传输破坏：混入空白换行、SPARK 前缀缺失（base64 大小写敏感，不动大小写）
        let dirty = format!("  {}\n", code.replacen("SPARK-", "", 1));
        let parsed = decode_conn_code(&dirty).unwrap();
        assert_eq!(parsed.peer_id, info.peer_id);
        assert_eq!(parsed.salt, info.salt);
        assert_eq!(verify_code(&info.salt), "a3f0c2");
    }

    #[test]
    fn frame_roundtrip() {
        let f = Frame::FileChunk {
            id: 1,
            seq: 3,
            data: vec![9u8; 300],
        };
        let bytes = postcard::to_allocvec(&f).unwrap();
        let back: Frame = postcard::from_bytes(&bytes).unwrap();
        match back {
            Frame::FileChunk { id, seq, data } => {
                assert_eq!((id, seq), (1, 3));
                assert_eq!(data.len(), 300);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn base64_hyphens_survive_new_and_legacy_codes() {
        for value in [0u32, 62, 255, 65535] {
            let info = ConnectInfo {
                addrs: vec!["/ip4/10.7.15.63/tcp/41300".into()],
                peer_id: "12D3KooWLmJWTDQ9XDFQQtL1yqKwttPxJY1ykWW5xT6L4xwC5hgJ".into(),
                salt: value.to_le_bytes(),
            };
            let code = encode_conn_code(&info).unwrap();
            let legacy = format!(
                "SPARK-{}",
                code[6..]
                    .as_bytes()
                    .chunks(12)
                    .map(|v| std::str::from_utf8(v).unwrap())
                    .collect::<Vec<_>>()
                    .join("-")
            );
            for input in [code, legacy] {
                let parsed = decode_conn_code(&input).unwrap();
                assert_eq!(parsed.salt, info.salt);
                assert_eq!(parsed.peer_id, info.peer_id);
                assert_eq!(parsed.addrs, info.addrs);
            }
        }
        assert!(decode_conn_code(&"x".repeat(8193)).is_err());
        assert!(decode_conn_code("SPARK-中文不是连接码").is_err());
    }
}

/// Independently versioned forum objects; existing chat frames are unchanged.
pub mod forum;
/// 独立图片协议；旧聊天 Frame 编码保持不变。
#[derive(Debug, Clone)]
pub struct ImageRequest {
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ImageReply {
    Accepted,
    Rejected(String),
}
