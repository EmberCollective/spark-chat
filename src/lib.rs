//! spark-chat —— 校园网 P2P 端到端加密聊天
//!
//! 五步功能：①骨干网确认(netpos) ②连接口令(net::conn_code) ③GUI聊天(ui)
//!          ④文件传输(protocol::FileChunk) ⑤本地加密(vault)
//!
//! 契约分层（并发开发时以本文件与 protocol.rs 为准，不得擅自更改签名）：
//! - vault.rs   加密库：金字塔 v2（口令⊕keyfile 解锁身份私钥 → ECDH 派生会话DEK → AEAD密文）
//! - net.rs     网络：libp2p 双模式（listen/dial），后台线程 + mpsc 与 UI 通信
//! - netpos.rs  网络位置检测（骨干直连/宿舍路由后/非校园网）
//! - ui.rs      界面契约（Telegram 式布局）
//! - protocol.rs 线上帧与连接码格式（唯一共享类型源）

pub mod netpos;
pub mod protocol;
pub mod vault;
pub mod net;
pub mod ui;

mod app;
mod desktop;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    desktop::run().expect("无法启动 Spark Chat");
}
pub mod transfer;

pub mod forum;
pub mod images;
