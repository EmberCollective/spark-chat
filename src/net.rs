//! 网络层 —— libp2p 双模式（监听/拨号），后台线程 + mpsc 与 UI 通信
//!
//! 技术选型：libp2p `request-response` 组件（工业件）承载自定义帧。
//! 每条 Frame = 一次请求（响应为空），底层自动获得 TCP+Noise+yamux 加密与多路复用。
//! 校内直连 = dial /ip4/10.x/tcp/端口；不配置任何 STUN/relay/DHT。
//! 未来跨网需要打洞时：在 SwarmBuilder 组装处引入 DCUtR 服务（此处刻意不用，校内直连足够）。
//!
//! @HELLO 内联控制帧约定：连接建立后双方立即互发
//! Frame::Text{seq:0, ts:0, text:"@HELLO <x25519公钥hex>"}；
//! net 层拦截不上抛 UI，解析出对方 x25519 公钥后发 NetEvent::PeerKey。

use crate::protocol::{self, ConnectInfo, Frame, CHUNK_SIZE};
use crate::transfer::ReceiveTarget;
use async_trait::async_trait;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use futures::StreamExt;
use libp2p::request_response::{
    Behaviour as RR, Codec as RRCodec, Config as RRConfig, Event as RREvent, Message,
    ProtocolSupport,
};
use libp2p::swarm::SwarmEvent;
use libp2p::{
    identity, multiaddr::Protocol as MaProtocol, Multiaddr, PeerId, StreamProtocol, SwarmBuilder,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read as _, Seek as _, Write as _};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

const PROTOCOL_NAME: &str = "/spark-chat/1.0.0";
const DEFAULT_PORT: u16 = 41300;
const HELLO_PREFIX: &str = "@HELLO ";
const MAX_INCOMING: usize = 2;

// ── UI 通信类型（契约，勿改） ──

#[derive(Debug)]
pub enum NetCmd {
    StartListen,
    To { peer_id: String, command: Box<NetCmd> },
    Dial(String),
    SendText(String),
    SendFile(PathBuf),
    // 系统文档选择器给出已授权文件句柄，避免在手机缓存明文副本。
    SendOpenedFile { name: String, file: std::fs::File },
    AcceptFile { id: u64, target: ReceiveTarget },
    RejectFile { id: u64 },
    Disconnect,
    Shutdown,
}

#[derive(Debug)]
pub enum NetEvent {
    PeerEvent { peer_id: String, event: Box<NetEvent> },
    ListenCode {
        code: String,
        verify: String,
    },
    PeerKey {
        x25519_pub: [u8; 32],
    },
    Connected(String),
    Disconnected(String),
    Msg(String),
    FileIncoming {
        id: u64,
        name: String,
        size: u64,
    },
    FileProgress {
        id: u64,
        done: u64,
        total: u64,
    },
    FileOutgoing {
        id: u64,
        name: String,
        size: u64,
    },
    FileSendProgress {
        id: u64,
        done: u64,
        total: u64,
    },
    FileReady {
        id: u64,
        name: String,
        sha256: String,
    },
    FileAcked {
        id: u64,
    },
    FileFailed {
        id: u64,
        outgoing: bool,
        reason: String,
    },
    Error(String),
}

/// 启动网络线程，返回 (命令通道, 事件通道)
pub fn spawn(
    ed25519_seed: [u8; 32],
    my_x25519_pub: [u8; 32],
) -> (mpsc::Sender<NetCmd>, mpsc::Receiver<NetEvent>) {
    spawn_mode(ed25519_seed, my_x25519_pub, false)
}

pub fn spawn_multi(ed25519_seed: [u8; 32], my_x25519_pub: [u8; 32]) -> (mpsc::Sender<NetCmd>, mpsc::Receiver<NetEvent>) {
    spawn_mode(ed25519_seed, my_x25519_pub, true)
}

fn spawn_mode(ed25519_seed: [u8; 32], my_x25519_pub: [u8; 32], multi: bool) -> (mpsc::Sender<NetCmd>, mpsc::Receiver<NetEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<NetCmd>();
    let (evt_tx, evt_rx) = mpsc::channel::<NetEvent>();
    std::thread::Builder::new()
        .name("spark-net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime 创建失败");
            rt.block_on(async_main(ed25519_seed, my_x25519_pub, cmd_rx, evt_tx, multi));
        })
        .expect("网络线程启动失败");
    (cmd_tx, evt_rx)
}

// ── 自定义 Codec：u32 BE 长度前缀 + postcard（request-response 组件要求） ──

#[derive(Debug, Clone)]
pub struct FrameCodec;

#[async_trait]
impl RRCodec for FrameCodec {
    type Protocol = StreamProtocol;
    type Request = Frame;
    type Response = ();

    async fn read_request<T>(&mut self, _p: &Self::Protocol, io: &mut T) -> io::Result<Frame>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut len = [0u8; 4];
        io.read_exact(&mut len).await?;
        let n = u32::from_be_bytes(len) as usize;
        if n > 16 * 1024 * 1024 {
            return Err(io::Error::other(format!("帧过大({n}B)")));
        }
        let mut buf = vec![0u8; n];
        io.read_exact(&mut buf).await?;
        postcard::from_bytes(&buf).map_err(|e| io::Error::other(e.to_string()))
    }

    async fn read_response<T>(&mut self, _p: &Self::Protocol, io: &mut T) -> io::Result<()>
    where
        T: AsyncRead + Unpin + Send,
    {
        // 响应为空：读到 EOF
        let mut buf = [0u8; 16];
        let _ = io.read(&mut buf).await;
        Ok(())
    }

    async fn write_request<T>(
        &mut self,
        _p: &Self::Protocol,
        io: &mut T,
        f: Frame,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let bytes = postcard::to_allocvec(&f).map_err(|e| io::Error::other(e.to_string()))?;
        io.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
        io.write_all(&bytes).await?;
        Ok(())
    }

    async fn write_response<T>(&mut self, _p: &Self::Protocol, io: &mut T, _r: ()) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        io.close().await?;
        Ok(())
    }
}

type Swarm = libp2p::Swarm<RR<FrameCodec>>;

const MAX_PEERS: usize = 8;

struct EventSink { sender: mpsc::Sender<NetEvent>, peer: Option<PeerId> }
impl EventSink {
    fn send(&self, event: NetEvent) -> Result<(), mpsc::SendError<NetEvent>> {
        self.sender.send(match self.peer {
            Some(peer) => NetEvent::PeerEvent { peer_id: peer.to_string(), event: Box::new(event) },
            None => event,
        })
    }
}

struct PeerState {
    peer: Option<PeerId>,
    pending_dial: Option<(ConnectInfo, usize)>,
    next_seq: u64,
    incoming: HashMap<u64, IncomingFile>,
    outgoing: Option<OutgoingFile>,
    texts: OutgoingTexts,
    preparing: Option<u64>,
}
impl Default for PeerState {
    fn default() -> Self { Self { peer: None, pending_dial: None, next_seq: 1, incoming: HashMap::new(), outgoing: None, texts: OutgoingTexts::default(), preparing: None } }
}

type PreparedFile = Result<(std::fs::File, u64, String, String), String>;
enum Action {
    Command(NetCmd),
    Network(SwarmEvent<RREvent<Frame, ()>>),
    Prepared(PeerId, u64, PreparedFile),
    Tick(PeerId),
}
fn event_peer(event: &SwarmEvent<RREvent<Frame, ()>>) -> Option<PeerId> {
    match event {
        SwarmEvent::ConnectionEstablished { peer_id, .. } | SwarmEvent::ConnectionClosed { peer_id, .. } => Some(*peer_id),
        SwarmEvent::OutgoingConnectionError { peer_id, .. } => *peer_id,
        SwarmEvent::Behaviour(RREvent::Message { peer, .. } | RREvent::OutboundFailure { peer, .. } | RREvent::InboundFailure { peer, .. } | RREvent::ResponseSent { peer, .. }) => Some(*peer),
        _ => None,
    }
}

// ── 主循环 ──

async fn async_main(
    ed25519_seed: [u8; 32],
    my_x25519_pub: [u8; 32],
    cmd_rx: mpsc::Receiver<NetCmd>,
    evt_tx: mpsc::Sender<NetEvent>,
    multi: bool,
) {
    // std mpsc → tokio mpsc 桥：UI 线程无阻塞发命令，async 侧统一 select
    let (tcmd_tx, mut tcmd_rx) = tokio::sync::mpsc::channel::<NetCmd>(16);
    tokio::task::spawn_blocking(move || {
        for cmd in cmd_rx {
            if tcmd_tx.blocking_send(cmd).is_err() {
                break;
            }
        }
    });

    // 身份：vault 解出的 ed25519 私钥种子 → libp2p Keypair（PeerId 与 vault 一致）
    let keypair =
        identity::Keypair::ed25519_from_bytes(ed25519_seed).expect("ed25519 seed 必须是 32 字节");

    let mut swarm: Swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            libp2p::tcp::Config::default(),
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )
        .expect("tcp+noise+yamux 组装失败")
        .with_behaviour(|_| {
            let protocols =
                std::iter::once((StreamProtocol::new(PROTOCOL_NAME), ProtocolSupport::Full));
            // 文件分块走请求，大文件给足请求超时
            let cfg = RRConfig::default().with_request_timeout(Duration::from_secs(300));
            RR::with_codec(FrameCodec, protocols, cfg)
        })
        .expect("request-response 构造失败")
        // 桌面聊天的停顿不应触发默认 10 秒空闲断线。
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(24 * 60 * 60)))
        .build();

    let mut evt_tx = EventSink { sender: evt_tx, peer: None };
    let my_id = *swarm.local_peer_id();
    let hello_text = format!(
        "{HELLO_PREFIX}{}",
        my_x25519_pub
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>()
    );

    let mut states: HashMap<PeerId, PeerState> = HashMap::new();
    let mut legacy_peer = None;
    let mut file_id_seed: u64 = 1;
    let mut listening = false;
    let (prepared_tx, mut prepared_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    let mut ticks = VecDeque::new();
    let port: u16 = std::env::var("SPARK_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    loop {
        let action = if let Some(p) = ticks.pop_front() { Action::Tick(p) } else {
            tokio::select! {
                cmd = tcmd_rx.recv() => match cmd { Some(cmd) => Action::Command(cmd), None => break },
                ev = swarm.select_next_some() => Action::Network(ev),
                Some((p, id, result)) = prepared_rx.recv() => Action::Prepared(p, id, result),
                _ = timer.tick() => { ticks.extend(states.keys().copied()); continue; }
            }
        };
        if let Action::Command(NetCmd::To { peer_id, command }) = &action {
            let valid = peer_id.parse::<PeerId>().ok().is_some_and(|id| states.get(&id).is_some_and(|state| state.peer.is_some()));
            if !valid || matches!(command.as_ref(), NetCmd::To { .. } | NetCmd::StartListen | NetCmd::Dial(_) | NetCmd::Shutdown) {
                evt_tx.peer = None;
                let _ = evt_tx.send(NetEvent::Error("目标会话未连接或命令无效".into()));
                continue;
            }
        }
        let target = match &action {
            Action::Command(NetCmd::Dial(code)) => protocol::decode_conn_code(code.trim()).ok().and_then(|i| i.peer_id.parse().ok()),
            Action::Command(NetCmd::To { peer_id, .. }) => peer_id.parse().ok(),
            Action::Network(ev) => event_peer(ev),
            Action::Prepared(p, ..) | Action::Tick(p) => Some(*p),
            _ => legacy_peer,
        };
        if let Some(p) = target {
            if !states.contains_key(&p) && states.len() >= MAX_PEERS {
                let _ = swarm.disconnect_peer_id(p);
                evt_tx.peer = None;
                let _ = evt_tx.send(NetEvent::Error("同时最多保留 8 个聊天会话，请锁定后重新连接".into()));
                continue;
            }
        }
        evt_tx.peer = if multi { target } else { None };
        let mut state = target.and_then(|p| states.remove(&p)).unwrap_or_default();
        let PeerState { mut peer, mut pending_dial, mut next_seq, mut incoming, mut outgoing, mut texts, mut preparing } = state;
        'dispatch: {
        match action {
            Action::Command(cmd) => {
                let cmd = match cmd { NetCmd::To { command, .. } => *command, other => other };
                let cmd = match cmd {
                    NetCmd::SendFile(path) => match std::fs::File::open(&path) {
                        Ok(file) => NetCmd::SendOpenedFile {
                            name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into()),
                            file,
                        },
                        Err(e) => { let _ = evt_tx.send(NetEvent::Error(format!("读文件失败：{e}"))); break 'dispatch; }
                    },
                    other => other,
                };
                match cmd {
                    NetCmd::StartListen => {
                        let listen_ma: Multiaddr = format!("/ip4/0.0.0.0/tcp/{port}").parse().unwrap();
                        if !listening {
                            if let Err(e) = swarm.listen_on(listen_ma) {
                                let _ = evt_tx.send(NetEvent::Error(format!("监听 {port} 失败：{e}（换端口：设 SPARK_PORT 环境变量）")));
                                break 'dispatch;
                            }
                            listening = true;
                        }
                        // 地址裁剪：10.x（骨干）优先，最多带 2 个，控制口令码长度
                        let mut addrs = local_ipv4s();
                        addrs.sort_by_key(|ip| if ip.octets()[0] == 10 { 0 } else { 1 });
                        let addrs = addrs.iter().take(2)
                            .map(|ip| format!("/ip4/{ip}/tcp/{port}"))
                            .collect::<Vec<_>>();
                        let info = ConnectInfo { addrs, peer_id: my_id.to_string(), salt: rand_bytes4() };
                        match protocol::encode_conn_code(&info) {
                            Ok(code) => {
                                let _ = evt_tx.send(NetEvent::ListenCode {
                                    verify: protocol::verify_code(&info.salt),
                                    code,
                                });
                            }
                            Err(e) => { let _ = evt_tx.send(NetEvent::Error(format!("生成口令码失败：{e}"))); }
                        }
                    }
                    NetCmd::Dial(code) => {
                        if peer.is_some() {
                            let _ = evt_tx.send(NetEvent::Error("当前已连接好友，请返回聊天；更换好友前请先锁定再解锁".into()));
                            break 'dispatch;
                        }
                        match protocol::decode_conn_code(code.trim()) {
                            Ok(info) => {
                                let Ok(target) = info.peer_id.parse::<PeerId>() else {
                                    let _ = evt_tx.send(NetEvent::Error("口令码里的身份信息无效，请重新复制整段".into()));
                                    break 'dispatch;
                                };
                                if info.addrs.is_empty() {
                                    let _ = evt_tx.send(NetEvent::Error("口令码里没有可用地址".into()));
                                    break 'dispatch;
                                }
                                // 逐个尝试：先拨第一个，失败轮换（见 OutgoingConnectionError 分支）
                                if let Some(idx) = try_dial(&mut swarm, &info, 0, target) {
                                    pending_dial = Some((info, idx));
                                } else {
                                    let _ = evt_tx.send(NetEvent::Error("连接码没有可拨号的地址，请重新复制".into()));
                                }
                            }
                            Err(e) => { let _ = evt_tx.send(NetEvent::Error(e.to_string())); }
                        }
                    }
                    NetCmd::SendText(text) => {
                        match peer {
                            Some(_) => {
                                match texts.enqueue(next_seq, text) {
                                    Ok(()) => next_seq += 1,
                                    Err(e) => { let _ = evt_tx.send(NetEvent::Error(e.into())); }
                                }
                            }
                            None => { let _ = evt_tx.send(NetEvent::Error("还没连上对方，消息发不出去".into())); }
                        }
                    }
                    NetCmd::SendFile(_) => unreachable!("已转成文件句柄"),
                    NetCmd::SendOpenedFile { name, file } => {
                        let Some(_) = peer else {
                            let _ = evt_tx.send(NetEvent::Error("还没连上对方，文件发不出去".into()));
                            break 'dispatch;
                        };
                        if outgoing.is_some() {
                            let _ = evt_tx.send(NetEvent::Error("已有文件正在发送，请完成后再选下一个".into()));
                            break 'dispatch;
                        }
                        let id = file_id_seed; file_id_seed += 1;
                        if preparing.is_some() { let _ = evt_tx.send(NetEvent::Error("已有文件正在准备，请稍后再发".into())); break 'dispatch; }
                        let size = match file.metadata() {
                            Ok(m) if m.is_file() && m.len() <= i64::MAX as u64 => m.len(),
                            _ => { let _ = evt_tx.send(NetEvent::Error("无法读取文件大小，请选择本机普通文件".into())); break 'dispatch; }
                        };
                        preparing = Some(id);
                        let _ = evt_tx.send(NetEvent::FileOutgoing { id, name: name.clone(), size });
                        let tx = prepared_tx.clone();
                        let prepared_peer = peer.unwrap();
                        tokio::task::spawn_blocking(move || {
                            let result = prepare_file(file, size).map(|(file, sha)| (file, size, name, sha));
                            let _ = tx.send((prepared_peer, id, result));
                        });
                    }
                    NetCmd::AcceptFile { id, target } => {
                        if let Some(f) = incoming.get_mut(&id) {
                            if f.target.is_some() { break 'dispatch; }
                            f.target = Some(target);
                            f.updated = std::time::Instant::now();
                            if let Some(channel) = f.channel.take() { let _ = swarm.behaviour_mut().send_response(channel, ()); }
                            let _ = evt_tx.send(NetEvent::FileProgress { id, done: 0, total: f.size });
                            if f.size == 0 { finish_incoming(id, &mut incoming, &evt_tx, &mut swarm, peer.unwrap()); }
                        }
                    }
                    NetCmd::RejectFile { id } => {
                        if release_incoming(id, &mut incoming, &mut swarm).is_some() {
                            if let Some(p) = peer { swarm.behaviour_mut().send_request(&p, Frame::FileReject { id, reason: "接收方已取消接收".into() }); }
                            let _ = evt_tx.send(NetEvent::FileFailed { id, outgoing: false, reason: "已取消接收".into() });
                        }
                    }
                    NetCmd::Disconnect => { if let Some(p) = peer { let _ = swarm.disconnect_peer_id(p); } },
                    NetCmd::Shutdown => return,
                    NetCmd::To { .. } => { let _ = evt_tx.send(NetEvent::Error("嵌套会话命令无效".into())); },
                }
            }
            Action::Network(ev) => {
                match ev {
                    SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                        if !multi && legacy_peer.is_some() && legacy_peer != Some(peer_id) {
                            let _ = swarm.disconnect_peer_id(peer_id);
                            break 'dispatch;
                        }
                        if peer == Some(peer_id) { break 'dispatch; }
                        pending_dial = None;
                        peer = Some(peer_id);
                        legacy_peer = Some(peer_id);
                        // 立即互发 @HELLO（内联控制帧，交换 x25519 静态公钥）
                        swarm.behaviour_mut().send_request(&peer_id, Frame::Text {
                            seq: 0, ts: 0, text: hello_text.clone(),
                        });
                        let _ = evt_tx.send(NetEvent::Connected(peer_id.to_string()));
                    }
                    SwarmEvent::ConnectionClosed { peer_id, num_established, cause, .. } => {
                        if num_established == 0 && Some(peer_id) == peer {
                            peer = None; incoming.clear(); outgoing = None; preparing = None;
                            let queued = texts.clear();
                            if queued > 0 { let _ = evt_tx.send(NetEvent::Error(format!("连接断开，{queued} 条排队消息未确认发送，请重连后检查并重发"))); }
                            let _ = evt_tx.send(NetEvent::Disconnected(
                                cause.map(|c| c.to_string()).unwrap_or_else(|| "连接关闭".into())
                            ));
                        }
                    }
                    SwarmEvent::ListenerError { error, .. } => {
                        listening = false;
                        let _ = evt_tx.send(NetEvent::Error(format!("监听失败：{error}")));
                    }
                    SwarmEvent::OutgoingConnectionError { error, .. } => {
                        // 拨号失败 → 轮换口令码里的下一个地址
                        if let Some((info, idx)) = pending_dial.clone() {
                            let next = idx + 1;
                            if next < info.addrs.len() {
                                if let Ok(target) = info.peer_id.parse::<PeerId>() {
                                    if let Some(idx) = try_dial(&mut swarm, &info, next, target) {
                                        pending_dial = Some((info, idx));
                                        break 'dispatch;
                                    }
                                }
                            }
                            pending_dial = None;
                            let _ = evt_tx.send(NetEvent::Error(format!("所有地址都连不上：{error}。请确认对方在线/在同一校园网")));
                        }
                    }
                    SwarmEvent::Behaviour(RREvent::Message { peer: peer_id, message: Message::Request { request, channel, .. }, .. }) => {
                        if Some(peer_id) != peer { break 'dispatch; }
                        if let Frame::FileMeta { id, name, size, sha256 } = request {
                            if incoming.len() >= MAX_INCOMING || incoming.contains_key(&id) || size > i64::MAX as u64 || name.len() > 1024 || sha256.len() != 64 || !sha256.bytes().all(|c| c.is_ascii_hexdigit()) {
                                let _ = swarm.behaviour_mut().send_response(channel, ());
                                swarm.behaviour_mut().send_request(&peer_id, Frame::FileReject { id, reason: "接收任务已满或文件信息无效".into() });
                                break 'dispatch;
                            }
                            incoming.insert(id, IncomingFile { name: name.clone(), size, sha: sha256, target: None, hasher: Sha256::new(), received: 0, next: 0, channel: Some(channel), updated: std::time::Instant::now() });
                            let _ = evt_tx.send(NetEvent::FileIncoming { id, name, size });
                        } else {
                            let _ = swarm.behaviour_mut().send_response(channel, ());
                            if let Frame::FileReject { id, reason } = &request {
                                if outgoing.as_ref().is_some_and(|f| f.id == *id) { outgoing = None; let _ = evt_tx.send(NetEvent::FileFailed { id: *id, outgoing: true, reason: reason.clone() }); }
                            }
                            if let Frame::FileDone { id, sha256 } = &request {
                                if outgoing.as_ref().is_some_and(|f| f.id == *id && f.sha == *sha256 && f.offset == f.size) {
                                    outgoing = None;
                                    let _ = evt_tx.send(NetEvent::FileAcked { id: *id });
                                }
                            }
                            handle_frame(request, peer_id, &mut incoming, &evt_tx, &mut swarm);
                        }
                    }
                    SwarmEvent::Behaviour(RREvent::Message { message: Message::Response { request_id, .. }, .. }) => {
                        texts.complete(request_id);
                        if outgoing.as_ref().is_some_and(|f| f.request == request_id) {
                            let f = outgoing.as_mut().unwrap();
                            f.updated = std::time::Instant::now();
                            let _ = evt_tx.send(NetEvent::FileSendProgress { id: f.id, done: f.offset, total: f.size });
                            if f.offset < f.size {
                                if let Some(p) = peer {
                                    let len = (f.size - f.offset).min(CHUNK_SIZE as u64) as usize;
                                    let mut data = vec![0; len];
                                    match f.file.read_exact(&mut data) {
                                        Ok(()) => {
                                            let frame = Frame::FileChunk { id: f.id, seq: f.offset / CHUNK_SIZE as u64, data };
                                            f.offset += len as u64;
                                            f.request = swarm.behaviour_mut().send_request(&p, frame);
                                        }
                                        Err(e) => {
                                            let id = f.id; outgoing = None;
                                            let _ = evt_tx.send(NetEvent::FileFailed { id, outgoing: true, reason: format!("读取失败：{e}") });
                                            let _ = swarm.disconnect_peer_id(p);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    SwarmEvent::Behaviour(RREvent::OutboundFailure { request_id, error, .. }) => {
                        if texts.active.is_some_and(|(id,_)| id == request_id) {
                            let queued = texts.clear().saturating_sub(1);
                            let _ = evt_tx.send(NetEvent::Error(format!("一条消息的送达结果未知，后续 {queued} 条消息尚未发送：{error}。请检查后重发。")));
                            break 'dispatch;
                        }
                        if outgoing.as_ref().is_some_and(|f| f.request == request_id) { let id = outgoing.take().unwrap().id; let _ = evt_tx.send(NetEvent::FileFailed { id, outgoing: true, reason: error.to_string() }); }
                        let _ = evt_tx.send(NetEvent::Error(format!("发送失败：{error}")));
                    }
                    SwarmEvent::Behaviour(RREvent::InboundFailure { error, .. }) => {
                        let _ = evt_tx.send(NetEvent::Error(format!("接收异常：{error}")));
                    }
                    SwarmEvent::Behaviour(RREvent::ResponseSent { .. }) => {}
                    _ => {}
                }
            }
            Action::Prepared(_, id, result) => {
                if preparing != Some(id) { break 'dispatch; }
                preparing = None;
                match (peer, result) {
                    (Some(p), Ok((file, size, name, sha))) => {
                        let request = swarm.behaviour_mut().send_request(&p, Frame::FileMeta { id, name, size, sha256: sha.clone() });
                        outgoing = Some(OutgoingFile { id, file, size, sha, offset: 0, request, updated: std::time::Instant::now() });
                        let _ = evt_tx.send(NetEvent::FileSendProgress { id, done: 0, total: size });
                    }
                    (_, Err(reason)) => { let _ = evt_tx.send(NetEvent::FileFailed { id, outgoing: true, reason }); }
                    _ => {}
                }
            }
            Action::Tick(_) => {
                if outgoing.as_ref().is_some_and(|f| f.offset == f.size && f.updated.elapsed() > Duration::from_secs(120)) {
                    let id = outgoing.take().unwrap().id;
                    let _ = evt_tx.send(NetEvent::FileFailed { id, outgoing: true, reason: "未收到保存确认，请与对方核实后重新发送".into() });
                }
                let expired: Vec<_> = incoming.iter().filter(|(_,f)| f.updated.elapsed() > Duration::from_secs(if f.target.is_some() { 120 } else { 290 })).map(|(&id,_)| id).collect();
                for id in expired {
                    release_incoming(id, &mut incoming, &mut swarm);
                    if let Some(p) = peer { swarm.behaviour_mut().send_request(&p, Frame::FileReject { id, reason: "等待接收或传输超时".into() }); }
                    let _ = evt_tx.send(NetEvent::FileFailed { id, outgoing: false, reason: "等待接收或传输超时，请重新发送".into() });
                }
            }
        }
        }
        if let Some(p) = peer {
            texts.flush(&mut swarm, &p);
        }
        state = PeerState { peer, pending_dial, next_seq, incoming, outgoing, texts, preparing };
        if let Some(p) = target { states.insert(p, state); }
    }
}

/// 尝试拨 info.addrs[idx]，成功发起返回 true
fn try_dial(swarm: &mut Swarm, info: &ConnectInfo, start: usize, target: PeerId) -> Option<usize> {
    for (idx, address) in info.addrs.iter().enumerate().skip(start) {
        if let Ok(ma) = address.parse::<Multiaddr>() {
            if swarm.dial(ma.with(MaProtocol::P2p(target))).is_ok() {
                return Some(idx);
            }
        }
    }
    None
}

#[derive(Default)]
struct OutgoingTexts {
    pending: VecDeque<Frame>,
    active: Option<(libp2p::request_response::OutboundRequestId, usize)>,
    bytes: usize,
}
impl OutgoingTexts {
    fn enqueue(&mut self, seq: u64, text: String) -> Result<(), &'static str> {
        let count = self.pending.len() + usize::from(self.active.is_some());
        if text.len() > 1024 * 1024
            || count >= 64
            || text.len() > (2 * 1024 * 1024usize).saturating_sub(self.bytes)
        {
            return Err("消息发送队列已满或文字超过 1 MB，本条没有发送，请稍后重发");
        }
        self.bytes += text.len();
        self.pending.push_back(Frame::Text {
            seq,
            ts: now_ms(),
            text,
        });
        Ok(())
    }
    fn flush(&mut self, swarm: &mut Swarm, peer: &PeerId) {
        if self.active.is_none() {
            if let Some(frame) = self.pending.pop_front() {
                let size = match &frame {
                    Frame::Text { text, .. } => text.len(),
                    _ => unreachable!(),
                };
                self.active = Some((swarm.behaviour_mut().send_request(peer, frame), size));
            }
        }
    }
    fn complete(&mut self, id: libp2p::request_response::OutboundRequestId) {
        if let Some((active, size)) = self.active {
            if active == id {
                self.active = None;
                self.bytes -= size;
            }
        }
    }
    fn clear(&mut self) -> usize {
        let count = self.pending.len() + usize::from(self.active.is_some());
        self.pending.clear();
        self.active = None;
        self.bytes = 0;
        count
    }
}

fn prepare_file(mut file: std::fs::File, size: u64) -> Result<(std::fs::File, String), String> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0; CHUNK_SIZE];
    let mut read = 0u64;
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        read = read.checked_add(n as u64).ok_or("文件过大")?;
        if read > size {
            return Err("文件在读取时发生变化，请重新选择".into());
        }
        hash.update(&buffer[..n]);
    }
    if read != size {
        return Err("文件在读取时发生变化，请重新选择".into());
    }
    file.rewind().map_err(|e| e.to_string())?;
    Ok((file, format!("{:x}", hash.finalize())))
}
struct OutgoingFile {
    id: u64,
    file: std::fs::File,
    size: u64,
    sha: String,
    offset: u64,
    request: libp2p::request_response::OutboundRequestId,
    updated: std::time::Instant,
}
struct IncomingFile {
    name: String,
    size: u64,
    sha: String,
    target: Option<ReceiveTarget>,
    hasher: Sha256,
    received: u64,
    next: u64,
    channel: Option<libp2p::request_response::ResponseChannel<()>>,
    updated: std::time::Instant,
}
impl IncomingFile {
    fn push(&mut self, seq: u64, data: &[u8]) -> Result<bool, String> {
        let left = self.size.saturating_sub(self.received);
        if seq != self.next || data.is_empty() || data.len() != left.min(CHUNK_SIZE as u64) as usize
        {
            return Err("文件分块顺序或长度不正确，请重新发送".into());
        }
        let target = self.target.as_mut().ok_or("尚未同意接收文件")?;
        target
            .file
            .write_all(data)
            .map_err(|e| format!("写入失败（可能空间不足）：{e}"))?;
        self.hasher.update(data);
        self.received += data.len() as u64;
        self.next += 1;
        self.updated = std::time::Instant::now();
        Ok(self.received == self.size)
    }
}
fn release_incoming(
    id: u64,
    incoming: &mut HashMap<u64, IncomingFile>,
    swarm: &mut Swarm,
) -> Option<IncomingFile> {
    let mut file = incoming.remove(&id)?;
    // 正常取消也结束元数据请求，避免把响应通道析构当成接收异常。
    if let Some(channel) = file.channel.take() {
        let _ = swarm.behaviour_mut().send_response(channel, ());
    }
    Some(file)
}
fn finish_incoming(
    id: u64,
    incoming: &mut HashMap<u64, IncomingFile>,
    evt: &EventSink,
    swarm: &mut Swarm,
    from: PeerId,
) {
    let mut file = incoming.remove(&id).unwrap();
    let got = format!("{:x}", file.hasher.finalize());
    let result = if got != file.sha {
        Err("文件校验失败，请重新发送".to_owned())
    } else {
        file.target
            .take()
            .ok_or("保存位置已失效".to_owned())
            .and_then(|t| t.commit().map_err(|e| format!("保存失败：{e}")))
    };
    match result {
        Ok(()) => {
            let _ = evt.send(NetEvent::FileReady {
                id,
                name: file.name,
                sha256: got.clone(),
            });
            swarm
                .behaviour_mut()
                .send_request(&from, Frame::FileDone { id, sha256: got });
        }
        Err(reason) => {
            let _ = evt.send(NetEvent::FileFailed {
                id,
                outgoing: false,
                reason: reason.clone(),
            });
            swarm
                .behaviour_mut()
                .send_request(&from, Frame::FileReject { id, reason });
        }
    }
}
fn handle_frame(
    frame: Frame,
    from: PeerId,
    incoming: &mut HashMap<u64, IncomingFile>,
    evt: &EventSink,
    swarm: &mut Swarm,
) {
    match frame {
        Frame::Text { seq, ts, text } => {
            if seq == 0 && ts == 0 {
                if let Some(hex) = text.strip_prefix(HELLO_PREFIX) {
                    if hex.len() == 64 {
                        if let Ok(bytes) = hex_decode(hex) {
                            let mut pk = [0u8; 32];
                            pk.copy_from_slice(&bytes);
                            let _ = evt.send(NetEvent::PeerKey { x25519_pub: pk });
                            return;
                        }
                    }
                    let _ = evt.send(NetEvent::Error("对方的握手公钥无效".into()));
                    return;
                }
            }
            if text.len() <= 1024 * 1024 {
                let _ = evt.send(NetEvent::Msg(text));
            } else {
                let _ = evt.send(NetEvent::Error("收到的文字超过 1 MB，已拒绝".into()));
            }
        }
        Frame::FileMeta { .. } => unreachable!("元数据在接收确认前暂存响应"),
        Frame::FileChunk { id, seq, data } => {
            let Some(file) = incoming.get_mut(&id) else {
                return;
            };
            match file.push(seq, &data) {
                Err(reason) => {
                    release_incoming(id, incoming, swarm);
                    let _ = evt.send(NetEvent::FileFailed {
                        id,
                        outgoing: false,
                        reason: reason.clone(),
                    });
                    swarm
                        .behaviour_mut()
                        .send_request(&from, Frame::FileReject { id, reason });
                }
                Ok(complete) => {
                    let _ = evt.send(NetEvent::FileProgress {
                        id,
                        done: file.received,
                        total: file.size,
                    });
                    if complete {
                        finish_incoming(id, incoming, evt, swarm, from);
                    }
                }
            }
        }
        Frame::FileDone { .. } | Frame::FileReject { .. } => {}
    }
}

// ── 小工具 ──

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    if !s.is_ascii() || !s.len().is_multiple_of(2) {
        return Err(());
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| ()))
        .collect()
}

fn rand_bytes4() -> [u8; 4] {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

fn local_ipv4s() -> Vec<std::net::Ipv4Addr> {
    local_ip_address::list_afinet_netifas()
        .map(|v| {
            v.into_iter()
                .filter_map(|(_, ip)| match ip {
                    std::net::IpAddr::V4(v4) if !v4.is_loopback() => Some(v4),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── 同步帧读写（契约保留，测试/调试用） ──

pub fn write_frame<W: std::io::Write>(w: &mut W, f: &Frame) -> std::io::Result<()> {
    let bytes = postcard::to_allocvec(f).map_err(|e| std::io::Error::other(e.to_string()))?;
    w.write_all(&(bytes.len() as u32).to_be_bytes())?;
    w.write_all(&bytes)
}

pub fn read_frame<R: std::io::Read>(r: &mut R) -> anyhow::Result<Frame> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_be_bytes(len) as usize;
    if n > 16 * 1024 * 1024 {
        anyhow::bail!("帧过大({n}B)，疑似数据流错位");
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut mut_buf(&mut buf))?;
    Ok(postcard::from_bytes(&buf)?)
}

fn mut_buf<'a>(b: &'a mut Vec<u8>) -> &'a mut [u8] {
    b.as_mut_slice()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_hello_does_not_panic() {
        assert!(hex_decode(&format!("A€{}", "0".repeat(60))).is_err());
    }
    #[test]
    fn file_chunks_require_exact_order_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = IncomingFile {
            name: "f".into(),
            size: 3,
            sha: String::new(),
            target: Some(ReceiveTarget::path(&dir.path().join("received"), 3).unwrap()),
            hasher: Sha256::new(),
            received: 0,
            next: 0,
            channel: None,
            updated: std::time::Instant::now(),
        };
        assert!(f.push(1, b"abc").is_err());
        assert!(f.push(0, b"abcd").is_err());
        assert!(f.push(0, b"abc").unwrap());
        assert!(f.push(1, b"x").is_err());
    }
    #[test]
    fn outgoing_text_queue_caps_count_and_bytes_and_releases_memory() {
        let mut q = OutgoingTexts::default();
        for i in 0..64 {
            q.enqueue(i, "hello".into()).unwrap();
        }
        assert!(q.enqueue(64, "overflow".into()).is_err());
        assert_eq!(q.clear(), 64);
        assert_eq!(q.bytes, 0);
        assert!(q.pending.is_empty() && q.active.is_none());
        q.enqueue(0, "x".repeat(1024 * 1024)).unwrap();
        q.enqueue(1, "y".repeat(1024 * 1024)).unwrap();
        assert!(q.enqueue(2, "overflow".into()).is_err());
        assert_eq!(q.bytes, 2 * 1024 * 1024);
        q.clear();
        assert!(q.enqueue(0, "x".repeat(1024 * 1024 + 1)).is_err());
    }
}
