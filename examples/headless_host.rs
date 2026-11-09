//! 无头演示主机：不开 GUI，只跑网络层。
//! 用途：本地验收——GUI 加入后能看到消息流、回显、界面状态。
//!
//! 运行: SPARK_PORT=42400 cargo run --example headless_host
//! 行为: 监听 → 打印口令码 → 对方连入后自动发 4 条欢迎消息 → 之后回显对方每句话。

use spark_chat::net::{self, NetCmd, NetEvent};
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

fn main() {
    let seed: [u8; 32] = (0u8..32).collect::<Vec<_>>().try_into().unwrap();
    let my_pub = [0x5A; 32]; // 演示用静态公钥
    let (cmd, evt) = net::spawn(seed, my_pub);

    cmd.send(NetCmd::StartListen).unwrap();

    let mut code_printed = false;
    let mut welcomed = false;
    let mut counter = 0u64;

    loop {
        match evt.try_recv() {
            Ok(ev) => match ev {
                NetEvent::ListenCode { code, verify } => {
                    println!("\n══════════════════════════════════════════");
                    println!("  连接口令码（粘进 GUI 的「加入聊天」）：");
                    println!("  {code}");
                    println!("  核对码: {verify}");
                    println!("══════════════════════════════════════════\n");
                    code_printed = true;
                }
                NetEvent::Connected(p) => {
                    println!("[主机] 对方已连入: {p}");
                    // 连上后依次发几条欢迎消息，展示消息流
                    let msgs = [
                        "欢迎连入 Spark Chat！我是无头演示主机 🤖",
                        "这条连接走的是 libp2p + Noise 加密，全程没有服务器",
                        "你现在看到的每条消息都已加密存进你的本地库（密文落盘）",
                        "随便打一句话，我会回显给你——测完直接关窗口即可",
                    ];
                    for m in msgs {
                        cmd.send(NetCmd::SendText(m.into())).unwrap();
                        std::thread::sleep(Duration::from_millis(400));
                    }
                    welcomed = true;
                }
                NetEvent::Msg(t) => {
                    println!("[收到] {t}");
                    let reply = format!("[回显] 你说了：{t}");
                    cmd.send(NetCmd::SendText(reply)).unwrap();
                }
                NetEvent::FileIncoming { id, .. } => {
                    cmd.send(NetCmd::RejectFile { id }).unwrap();
                    println!("[主机] 此演示节点只回显文字，已拒绝附件；传文件请使用完整客户端。");
                }
                NetEvent::PeerKey { .. } => {}
                NetEvent::Disconnected(r) => println!("[主机] 断开: {r}"),
                NetEvent::Error(e) => println!("[主机] 错误: {e}"),
                other => println!("[主机] 事件: {}", tag(&other)),
            },
            Err(TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(100));
                if code_printed && !welcomeded_hint(welcomed, counter) {
                    // 空转等待；每 10 秒打一个等待点
                    counter += 1;
                }
            }
            Err(TryRecvError::Disconnected) => break,
        }
    }
}

fn welcomeded_hint(welcomed: bool, c: u64) -> bool {
    if !welcomed && c % 100 == 0 && c > 0 {
        println!("…等待 GUI 加入…");
    }
    welcomed
}

fn tag(e: &NetEvent) -> &'static str {
    match e {
        NetEvent::PeerEvent { .. } => "PeerEvent",
        NetEvent::ImageIncoming { .. } => "ImageIncoming",
            NetEvent::ImageDelivered => "ImageDelivered",
        NetEvent::ListenCode { .. } => "ListenCode",
        NetEvent::PeerKey { .. } => "PeerKey",
        NetEvent::Connected(_) => "Connected",
        NetEvent::Disconnected(_) => "Disconnected",
        NetEvent::Msg(_) => "Msg",
        NetEvent::FileIncoming { .. } => "FileIncoming",
        NetEvent::FileOutgoing { .. } => "FileOutgoing",
        NetEvent::FileSendProgress { .. } => "FileSendProgress",
        NetEvent::FileProgress { .. } => "FileProgress",
        NetEvent::FileReady { .. } => "FileReady",
        NetEvent::FileAcked { .. } => "FileAcked",
        NetEvent::Error(_) => "Error",
        NetEvent::FileFailed { .. } => "FileFailed",
    }
}
