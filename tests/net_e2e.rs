//! 端到端联调：同进程拉起两个真实 libp2p 节点，
//! 走完整用户路径：生成口令码 → 粘码拨号 → Noise 握手 → @HELLO 互换公钥 → 发消息。
//! 这是产品级验收：任何一步断了都能在这里暴露。

use spark_chat::net::{self, NetCmd, NetEvent};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

fn recv_until(
    rx: &std::sync::mpsc::Receiver<NetEvent>,
    pred: &str,
    timeout: Duration,
) -> Vec<NetEvent> {
    let start = Instant::now();
    let mut seen = Vec::new();
    while start.elapsed() < timeout {
        match rx.try_recv() {
            Ok(ev) => {
                if let NetEvent::Error(e) = &ev {
                    panic!("network error waiting for {pred}: {e}");
                }
                let hit = match &ev {
                    NetEvent::ListenCode { .. } => pred == "listen",
                    NetEvent::Connected(_) => pred == "connected",
                    NetEvent::PeerKey { .. } => pred == "peerkey",
                    NetEvent::Msg(t) => pred == "msg" && t == "e2e你好，这是端到端测试消息",
                    _ => false,
                };
                seen.push(ev);
                if hit {
                    return seen;
                }
            }
            Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(50)),
            Err(TryRecvError::Disconnected) => panic!("网络线程已退出"),
        }
    }
    panic!(
        "等待 {pred} 超时，收到的事件: {:?}",
        seen.iter().map(|e| e.tag()).collect::<Vec<_>>()
    );
}

// 事件打个小标签方便断言失败时人读
trait Tag {
    fn tag(&self) -> &'static str;
}
impl Tag for NetEvent {
    fn tag(&self) -> &'static str {
        match self {
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
}

#[test]
fn two_nodes_full_path() {
    // 独立端口和回环地址，避免占用用户正在运行的客户端或 VPN 路由。
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    std::env::set_var("SPARK_PORT", port.to_string());
    // 两个随机身份（模拟两台不同设备）
    let seed_a: [u8; 32] = (0u8..32).collect::<Vec<_>>().try_into().unwrap();
    let seed_b: [u8; 32] = (32u8..64).collect::<Vec<_>>().try_into().unwrap();

    let (cmd_a, evt_a) = net::spawn(seed_a, [0xAA; 32]);
    let (cmd_b, evt_b) = net::spawn(seed_b, [0xBB; 32]);

    // ① A 创建聊天（监听）→ 拿到口令码
    cmd_a.send(NetCmd::StartListen).unwrap();
    let code = {
        let evs = recv_until(&evt_a, "listen", Duration::from_secs(10));
        evs.into_iter()
            .find_map(|e| match e {
                NetEvent::ListenCode { code, .. } => Some(code),
                _ => None,
            })
            .expect("应有连接码")
    };
    assert!(code.starts_with("SPARK-"), "码格式: {code}");

    // ② B 粘码加入（真实拨号，走本机局域网地址）
    let mut info = spark_chat::protocol::decode_conn_code(&code).unwrap();
    info.addrs = vec![format!("/ip4/127.0.0.1/tcp/{port}")];
    cmd_b
        .send(NetCmd::Dial(
            spark_chat::protocol::encode_conn_code(&info).unwrap(),
        ))
        .unwrap();
    let _ = recv_until(&evt_b, "connected", Duration::from_secs(15));
    let _ = recv_until(&evt_a, "connected", Duration::from_secs(5));
    let _ = recv_until(&evt_b, "peerkey", Duration::from_secs(5));
    let _ = recv_until(&evt_a, "peerkey", Duration::from_secs(5));

    // ③ A 发消息 → B 收到（Noise 加密链路 + @HELLO 过滤正确）
    cmd_a
        .send(NetCmd::SendText("e2e你好，这是端到端测试消息".into()))
        .unwrap();
    recv_until(&evt_b, "msg", Duration::from_secs(10));

    // ④ 两个方向使用相同 id，文件仍应各自完整到达；每个文件跨多个分块。
    let tmp = tempfile::tempdir().unwrap();
    let a_data = vec![0xA5; 72 * 1024 * 1024 + 19];
    let b_data = vec![0x5A; spark_chat::protocol::CHUNK_SIZE + 7];
    let a_path = tmp.path().join("from-a.bin");
    let b_path = tmp.path().join("from-b.bin");
    std::fs::write(&a_path, &a_data).unwrap();
    std::fs::write(&b_path, &b_data).unwrap();
    cmd_a.send(NetCmd::SendFile(a_path)).unwrap();
    cmd_b
        .send(NetCmd::SendOpenedFile {
            name: "from-b.bin".into(),
            file: std::fs::File::open(b_path).unwrap(),
        })
        .unwrap();
    let a_burst = (0..24)
        .map(|n| format!("A ordered message {n:02}"))
        .collect::<Vec<_>>();
    let b_burst = (0..24)
        .map(|n| format!("B ordered message {n:02}"))
        .collect::<Vec<_>>();
    for (a, b) in a_burst.iter().zip(&b_burst) {
        cmd_a.send(NetCmd::SendText(a.clone())).unwrap();
        cmd_b.send(NetCmd::SendText(b.clone())).unwrap();
    }
    let mut a_received = Vec::new();
    let mut b_received = Vec::new();
    let start = Instant::now();
    let mut ready_a = false;
    let mut ready_b = false;
    let mut sent_a = false;
    let mut sent_b = false;
    let mut delayed_acceptance_checked = false;
    while !(ready_a
        && ready_b
        && sent_a
        && sent_b
        && a_received.len() == 24
        && b_received.len() == 24)
        && start.elapsed() < Duration::from_secs(60)
    {
        for (rx, own) in [(&evt_a, true), (&evt_b, false)] {
            while let Ok(event) = rx.try_recv() {
                match event {
                    NetEvent::Msg(text) => {
                        if own {
                            a_received.push(text);
                        } else {
                            b_received.push(text);
                        }
                    }
                    NetEvent::FileIncoming { id, size, .. } => {
                        if !delayed_acceptance_checked {
                            std::thread::sleep(Duration::from_secs(15));
                            delayed_acceptance_checked = true;
                        }
                        let path = tmp
                            .path()
                            .join(if own { "received-b" } else { "received-a" });
                        let target =
                            spark_chat::transfer::ReceiveTarget::path(&path, size).unwrap();
                        (if own { &cmd_a } else { &cmd_b })
                            .send(NetCmd::AcceptFile { id, target })
                            .unwrap();
                    }
                    NetEvent::FileReady { .. } => {
                        if own {
                            assert_eq!(
                                std::fs::read(tmp.path().join("received-b")).unwrap(),
                                b_data
                            );
                            ready_a = true;
                        } else {
                            assert_eq!(
                                std::fs::read(tmp.path().join("received-a")).unwrap(),
                                a_data
                            );
                            ready_b = true;
                        }
                    }
                    NetEvent::FileSendProgress { done, total, .. } if done == total => {
                        if own {
                            sent_a = true;
                        } else {
                            sent_b = true;
                        }
                    }
                    NetEvent::Error(e) => panic!("file transfer failed: {e}"),
                    _ => {}
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        ready_a && ready_b && sent_a && sent_b,
        "both directions must finish"
    );
    assert_eq!(
        a_received, b_burst,
        "B burst must arrive in send order while transferring files"
    );
    assert_eq!(
        b_received, a_burst,
        "A burst must arrive in send order while transferring files"
    );
    // 空文件没有分块，元数据本身必须完成接收。
    let empty = tmp.path().join("empty.txt");
    std::fs::write(&empty, []).unwrap();
    cmd_a.send(NetCmd::SendFile(empty)).unwrap();
    let start = Instant::now();
    let mut empty_ready = false;
    while start.elapsed() < Duration::from_secs(5) {
        match evt_b.try_recv() {
            Ok(NetEvent::FileIncoming { id, size, .. }) => {
                assert_eq!(size, 0);
                let target = spark_chat::transfer::ReceiveTarget::path(
                    &tmp.path().join("empty-received"),
                    size,
                )
                .unwrap();
                cmd_b.send(NetCmd::AcceptFile { id, target }).unwrap();
            }
            Ok(NetEvent::FileReady { .. }) => {
                assert!(std::fs::read(tmp.path().join("empty-received"))
                    .unwrap()
                    .is_empty());
                empty_ready = true;
                break;
            }
            Ok(NetEvent::Error(e)) => panic!("empty file failed: {e}"),
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    assert!(empty_ready, "empty file must complete");

    // 接收方拒绝元数据后，发送方收到明确结果，连接仍能使用。
    let reject = tmp.path().join("reject.txt");
    std::fs::write(&reject, b"reject me").unwrap();
    cmd_a.send(NetCmd::SendFile(reject)).unwrap();
    let start = Instant::now();
    let mut rejected = false;
    while start.elapsed() < Duration::from_secs(5) {
        match evt_b.try_recv() {
            Ok(NetEvent::FileIncoming { id, .. }) => cmd_b.send(NetCmd::RejectFile { id }).unwrap(),
            Ok(NetEvent::Error(error)) => panic!("正常取消不能报接收异常：{error}"),
            _ => {}
        }
        if let Ok(NetEvent::FileFailed { outgoing: true, .. }) = evt_a.try_recv() {
            rejected = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(rejected);
    while evt_b.try_recv().is_ok() {}

    // 超过 libp2p 默认的 10 秒空闲期限后，聊天连接仍保持可用。
    std::thread::sleep(Duration::from_secs(12));
    for rx in [&evt_a, &evt_b] {
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, NetEvent::Disconnected(_) | NetEvent::Error(_)),
                "idle connection should remain open: {event:?}"
            );
        }
    }
    cmd_a
        .send(NetCmd::SendText("e2e你好，这是端到端测试消息".into()))
        .unwrap();
    recv_until(&evt_b, "msg", Duration::from_secs(5));

    // ⑤ 双方收工
    cmd_a.send(NetCmd::Shutdown).unwrap();
    cmd_b.send(NetCmd::Shutdown).unwrap();
}
