//! Independent forum worker. All mutable state and disk writes have one owner.
use super::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures::StreamExt;
use libp2p::{
    multiaddr::Protocol,
    request_response::{Event, Message, OutboundRequestId},
    swarm::SwarmEvent,
    Multiaddr, PeerId,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ForumRequest {
    State,
    Create {
        name: String,
    },
    Join {
        #[serde(alias = "code")]
        invitation: String,
    },
    Listen,
    Connect {
        code: String,
    },
    Admit {
        member: String,
        #[serde(default)]
        inviter: Option<String>,
        #[serde(default)]
        expires_at: Option<u64>,
    },
    Revoke {
        member: String,
    },
    Moderate {
        target: String,
        hidden: bool,
    },
    Checkpoint {
        records: Vec<String>,
    },
    Publish {
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        reply_to: Option<String>,
        #[serde(default)]
        parents: Vec<String>,
        title: String,
        body: String,
    },
    Fetch {
        hash: String,
    },
    Sync,
}
struct Command {
    request: ForumRequest,
    reply: mpsc::SyncSender<Result<Value>>,
}
pub struct ForumService {
    tx: mpsc::SyncSender<Command>,
}
impl ForumService {
    pub fn start(seed: [u8; 32], storage_key: [u8; 32], directory: PathBuf) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel(16);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("spark-forum".into())
            .spawn(move || {
                let result = (|| -> Result<_> {
                    let key = Keypair::ed25519_from_bytes(seed)?;
                    Worker::new(key, storage_key, directory)
                })();
                match result {
                    Ok(worker) => {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build();
                        match rt {
                            Ok(rt) => rt.block_on(async move {
                                match super::wire::swarm(worker.key.clone()) {
                                    Ok(swarm) => {
                                        let _ = ready_tx.send(Ok(()));
                                        worker.run(swarm, rx).await;
                                    }
                                    Err(e) => {
                                        let _ = ready_tx.send(Err(e));
                                    }
                                }
                            }),
                            Err(e) => {
                                let _ = ready_tx.send(Err(e.into()));
                            }
                        }
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })?;
        ready_rx.recv_timeout(Duration::from_secs(10))??;
        Ok(Self { tx })
    }
    /// Call from a blocking task, never while holding the application's UI mutex.
    pub fn handle(&self, request: ForumRequest) -> Result<Value> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx
            .try_send(Command { request, reply: tx })
            .map_err(|_| anyhow::anyhow!("论坛操作排队已满或已锁定"))?;
        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|_| anyhow::anyhow!("论坛操作结果未知，请先刷新状态"))?
    }
}
#[derive(Serialize, Deserialize)]
struct Saved {
    name: String,
    forum: Forum,
}
#[derive(Serialize, Deserialize)]
struct Invitation {
    network: Digest,
    admin: Vec<u8>,
    member: Vec<u8>,
    name: String,
    governance: Vec<SignedRecord>,
}
#[derive(Serialize, Deserialize)]
struct Entry {
    network: Digest,
    peer: String,
    addresses: Vec<String>,
}
struct Pending {
    peer: PeerId,
    query: Query,
}
struct Worker {
    key: Keypair,
    storage_key: [u8; 32],
    path: PathBuf,
    saved: Option<Saved>,
    view: ForumView,
    listen: Vec<Multiaddr>,
    listening: bool,
    peers: BTreeSet<PeerId>,
    requests: HashMap<OutboundRequestId, Pending>,
    queues: HashMap<PeerId, VecDeque<Query>>,
    rates: HashMap<PeerId, (Instant, u32)>,
    dirty: bool,
    status: String,
}
pub fn hex(hash: &Digest) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn parse_hash(text: &str) -> Result<Digest> {
    ensure!(
        text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()),
        "内容标识格式无效"
    );
    let mut out = [0; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn decode_public(text: &str) -> Result<Vec<u8>> {
    ensure!(text.len() <= 128, "公开身份过长");
    let bytes = URL_SAFE_NO_PAD.decode(text.trim())?;
    public(&bytes)?;
    Ok(bytes)
}
fn encode<T: Serialize>(prefix: &str, value: &T) -> Result<String> {
    Ok(format!(
        "{prefix}{}",
        URL_SAFE_NO_PAD.encode(postcard::to_allocvec(value)?)
    ))
}
fn decode<T: serde::de::DeserializeOwned>(prefix: &str, code: &str) -> Result<T> {
    ensure!(code.len() <= 512 * 1024, "连接或邀请材料过大");
    let bytes = URL_SAFE_NO_PAD.decode(
        code.trim()
            .strip_prefix(prefix)
            .ok_or_else(|| anyhow::anyhow!("材料类型不匹配"))?,
    )?;
    Ok(postcard::from_bytes(&bytes)?)
}
fn campus_address(addr: &Multiaddr) -> bool {
    let parts: Vec<_> = addr.iter().collect();
    matches!(parts.as_slice(), [Protocol::Ip4(ip), Protocol::Tcp(port)] if *port > 0 && (ip.is_private() || ip.is_loopback()))
}
impl Worker {
    fn new(key: Keypair, storage_key: [u8; 32], directory: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("forum.enc");
        let saved = if path.exists() {
            let value: Saved =
                postcard::from_bytes(&super::storage::load_bytes(&path, &storage_key)?)?;
            Some(Saved {
                name: value.name,
                forum: super::storage::validate_snapshot(value.forum)?,
            })
        } else {
            None
        };
        let view = saved
            .as_ref()
            .map(|s| s.forum.rebuild())
            .unwrap_or_default();
        Ok(Self {
            key,
            storage_key,
            path,
            saved,
            view,
            listen: vec![],
            listening: false,
            peers: BTreeSet::new(),
            requests: HashMap::new(),
            queues: HashMap::new(),
            rates: HashMap::new(),
            dirty: false,
            status: String::new(),
        })
    }
    fn refresh(&mut self) {
        if let Some(s) = &self.saved {
            self.view = s.forum.rebuild();
        }
        self.dirty = true;
    }
    fn persist(&mut self) -> Result<()> {
        if self.dirty {
            if let Some(s) = &self.saved {
                super::storage::save_bytes(
                    &self.path,
                    &self.storage_key,
                    &postcard::to_allocvec(s)?,
                )?;
            }
            self.dirty = false;
        }
        Ok(())
    }
    fn allowed(&self, peer: &PeerId) -> bool {
        !self.view.governance_conflict
            && self.view.members.iter().any(|(p, m)| {
                !m.revoked
                    && m.expires_at > now()
                    && public(p).is_ok_and(|p| p.to_peer_id() == *peer)
            })
    }
    fn governance(&self) -> Vec<SignedRecord> {
        let mut records: Vec<_> = self
            .saved
            .as_ref()
            .map(|s| {
                s.forum
                    .records
                    .values()
                    .filter(|r| matches!(r.record.action, Action::Governance { .. }))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        records.sort_by_key(|r| match r.record.action {
            Action::Governance { sequence, .. } => sequence,
            _ => 0,
        });
        records.truncate(256);
        records
    }
    fn state(&self) -> Value {
        let own = self.key.public().encode_protobuf();
        let mut topics = vec![];
        if let Some(s) = &self.saved {
            for topic in &self.view.topics {
                let root = &s.forum.records[&topic.root];
                let mut versions = vec![];
                for (id, r) in &s.forum.records {
                    if let Action::Content {
                        target,
                        body,
                        attachments,
                        ..
                    } = &r.record.action
                    {
                        if target.unwrap_or(*id) != topic.root {
                            continue;
                        }
                        versions.push(json!({"id":hex(id),"author":URL_SAFE_NO_PAD.encode(&r.record.author),"body":s.forum.body(body).and_then(|b|std::str::from_utf8(b).ok()),"body_hash":hex(body),"disputed":self.view.disputed.contains(id),"attachments":attachments.iter().map(|a|json!({"name":a.name,"size":a.size,"hash":hex(&a.hash)})).collect::<Vec<_>>()}));
                    }
                }
                let title = topic
                    .heads
                    .first()
                    .and_then(|id| s.forum.records.get(id))
                    .and_then(|r| {
                        if let Action::Content { title, .. } = &r.record.action {
                            Some(title)
                        } else {
                            None
                        }
                    });
                topics.push(json!({"id":hex(&topic.root),"title":title,"author":URL_SAFE_NO_PAD.encode(&root.record.author),"reply_to":topic.reply_to.map(|h|hex(&h)),"hidden":topic.hidden,"heads":topic.heads.iter().map(hex).collect::<Vec<_>>(),"versions":versions}));
            }
        }
        json!({"joined":self.saved.is_some(),"admin":self.saved.as_ref().is_some_and(|s|s.forum.admin==own),"name":self.saved.as_ref().map(|s|s.name.as_str()).unwrap_or(""),"own_id":self.key.public().to_peer_id().to_string(),"own_public":URL_SAFE_NO_PAD.encode(own),"peers":self.peers.len(),"pending":self.view.pending.len(),"disputed":self.view.disputed.len(),"governance_conflict":self.view.governance_conflict,"topics":topics,"members":self.view.members.iter().map(|(p,m)|json!({"id":URL_SAFE_NO_PAD.encode(p),"active":!m.revoked&&m.expires_at>now()})).collect::<Vec<_>>(),"status":self.status})
    }
    fn apply_governance(&mut self, records: Vec<SignedRecord>) -> Result<()> {
        ensure!(records.len() <= 256, "治理记录过多");
        let saved = self
            .saved
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("尚未加入论坛"))?;
        for record in records {
            ensure!(
                matches!(record.record.action, Action::Governance { .. }),
                "身份握手仅接受治理记录"
            );
            saved.forum.ingest(record)?;
        }
        self.refresh();
        Ok(())
    }
    fn govern(&mut self, change: GovernanceChange) -> Result<()> {
        let s = self
            .saved
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("尚未加入论坛"))?;
        ensure!(
            s.forum.admin == self.key.public().encode_protobuf(),
            "只有网络管理员可以执行此操作"
        );
        ensure!(
            !self.view.governance_conflict && self.view.governance_sequence < 256,
            "治理冲突或一期治理记录容量已满"
        );
        let r = sign_record(
            &self.key,
            s.forum.network,
            Action::Governance {
                sequence: self.view.governance_sequence + 1,
                previous: self.view.governance_head,
                change,
            },
        )?;
        let mut candidate = s.forum.clone();
        let id = candidate.ingest(r)?;
        ensure!(
            candidate.rebuild().governance_head == Some(id),
            "治理操作依赖尚未满足"
        );
        s.forum = candidate;
        self.refresh();
        Ok(())
    }
    fn entry(&self) -> Result<String> {
        let s = self
            .saved
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("尚未加入论坛"))?;
        ensure!(!self.listen.is_empty(), "正在准备监听地址，请稍后刷新");
        encode(
            "FORUM-CONNECT-",
            &Entry {
                network: s.forum.network,
                peer: self.key.public().to_peer_id().to_string(),
                addresses: self.listen.iter().map(ToString::to_string).collect(),
            },
        )
    }
    fn command(&mut self, request: ForumRequest, swarm: &mut super::wire::Swarm) -> Result<Value> {
        let mut code = None;
        match request {
            ForumRequest::State => {}
            ForumRequest::Create { name } => {
                ensure!(self.saved.is_none(), "已经加入论坛，不能覆盖本地论坛");
                ensure!(
                    !name.trim().is_empty() && name.chars().count() <= 100,
                    "论坛名称无效"
                );
                let mut network = [0; 32];
                rand::thread_rng().fill_bytes(&mut network);
                self.saved = Some(Saved {
                    name,
                    forum: Forum::new(network, self.key.public().encode_protobuf())?,
                });
                self.refresh();
            }
            ForumRequest::Join { invitation } => {
                ensure!(self.saved.is_none(), "已经加入论坛，不能覆盖本地论坛");
                let invite: Invitation = decode("FORUM-INVITE-", &invitation)?;
                ensure!(
                    invite.member == self.key.public().encode_protobuf()
                        && invite.governance.len() <= 256
                        && invite.name.len() <= 400,
                    "邀请不属于当前身份或材料超限"
                );
                let mut forum = Forum::new(invite.network, invite.admin)?;
                for r in invite.governance {
                    ensure!(
                        matches!(r.record.action, Action::Governance { .. }),
                        "邀请含非治理记录"
                    );
                    forum.ingest(r)?;
                }
                ensure!(
                    forum.can_access(&invite.member, now()),
                    "邀请未获批准、已过期或已撤销"
                );
                self.saved = Some(Saved {
                    name: invite.name,
                    forum,
                });
                self.refresh();
            }
            ForumRequest::Listen => {
                ensure!(self.saved.is_some(), "请先创建或加入论坛");
                if !self.listening {
                    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;
                    self.listening = true;
                }
                if !self.listen.is_empty() {
                    code = Some(self.entry()?);
                }
            }
            ForumRequest::Connect { code: input } => {
                let e: Entry = decode("FORUM-CONNECT-", &input)?;
                ensure!(
                    self.saved
                        .as_ref()
                        .is_some_and(|s| s.forum.network == e.network),
                    "不是当前论坛的连接码"
                );
                ensure!(e.addresses.len() <= 8, "连接地址过多");
                let peer: PeerId = e.peer.parse()?;
                ensure!(self.peers.len() < 8, "论坛邻居已达上限");
                for addr in e.addresses {
                    let mut addr: Multiaddr = addr.parse()?;
                    ensure!(campus_address(&addr), "仅接受校内私有 IPv4 地址");
                    addr.push(Protocol::P2p(peer));
                    swarm.dial(addr)?;
                }
            }
            ForumRequest::Admit {
                member,
                inviter,
                expires_at,
            } => {
                let member = decode_public(&member)?;
                let inviter = inviter
                    .as_deref()
                    .map(decode_public)
                    .transpose()?
                    .unwrap_or_else(|| self.key.public().encode_protobuf());
                let expires_at = expires_at.unwrap_or(now() + 30 * 86400);
                ensure!(expires_at > now(), "有效期必须晚于当前时间");
                self.govern(GovernanceChange::Admit {
                    member: member.clone(),
                    inviter,
                    expires_at,
                })?;
                let s = self.saved.as_ref().unwrap();
                code = Some(encode(
                    "FORUM-INVITE-",
                    &Invitation {
                        network: s.forum.network,
                        admin: s.forum.admin.clone(),
                        member,
                        name: s.name.clone(),
                        governance: self.governance(),
                    },
                )?);
            }
            ForumRequest::Revoke { member } => {
                self.govern(GovernanceChange::Revoke {
                    member: decode_public(&member)?,
                })?;
            }
            ForumRequest::Moderate { target, hidden } => {
                self.govern(GovernanceChange::Moderate {
                    target: parse_hash(&target)?,
                    hidden,
                })?;
            }
            ForumRequest::Checkpoint { records } => {
                let mut records = records
                    .iter()
                    .map(|s| parse_hash(s))
                    .collect::<Result<Vec<_>>>()?;
                records.sort();
                self.govern(GovernanceChange::Checkpoint { records })?;
            }
            ForumRequest::Publish {
                target,
                reply_to,
                parents,
                title,
                body,
            } => {
                ensure!(
                    self.allowed(&self.key.public().to_peer_id()),
                    "当前成员没有论坛发布资格"
                );
                let target = target.as_deref().map(parse_hash).transpose()?;
                let reply_to = reply_to.as_deref().map(parse_hash).transpose()?;
                let mut parents = parents
                    .iter()
                    .map(|s| parse_hash(s))
                    .collect::<Result<Vec<_>>>()?;
                parents.sort();
                let s = self.saved.as_mut().unwrap();
                let mut candidate = s.forum.clone();
                let body = candidate.put_body(body.into_bytes())?;
                let r = sign_record(
                    &self.key,
                    candidate.network,
                    Action::Content {
                        target,
                        reply_to,
                        parents,
                        title,
                        body,
                        attachments: vec![],
                    },
                )?;
                let id = candidate.ingest(r)?;
                let view = candidate.rebuild();
                ensure!(
                    !view.pending.contains(&id) && !view.disputed.contains(&id),
                    "发布依赖缺失或有撤销争议"
                );
                s.forum = candidate;
                self.refresh();
                self.status = "已在本机保存，正在同步".into();
            }
            ForumRequest::Fetch { hash } => {
                let hash = parse_hash(&hash)?;
                for peer in self.peers.clone() {
                    self.enqueue(peer, Query::Body { hash });
                }
            }
            ForumRequest::Sync => {
                for peer in self.peers.clone() {
                    self.enqueue(
                        peer,
                        Query::Hello {
                            governance: self.governance(),
                        },
                    );
                }
            }
        }
        self.persist()?;
        if code.is_none() && self.listening && !self.listen.is_empty() {
            code = Some(self.entry()?);
        }
        Ok(json!({"state":self.state(),"code":code}))
    }
    fn enqueue(&mut self, peer: PeerId, query: Query) {
        let queue = self.queues.entry(peer).or_default();
        if queue.len() < 128 {
            queue.push_back(query);
        }
    }
    fn next(&mut self, swarm: &mut super::wire::Swarm) {
        let Some(s) = &self.saved else { return };
        let network = s.forum.network;
        for peer in self.peers.clone() {
            if self.requests.values().any(|p| p.peer == peer) {
                continue;
            }
            if let Some(query) = self.queues.get_mut(&peer).and_then(VecDeque::pop_front) {
                let id = swarm.behaviour_mut().send_request(
                    &peer,
                    Request {
                        network,
                        query: query.clone(),
                    },
                );
                self.requests.insert(id, Pending { peer, query });
            }
        }
    }
    fn answer(&mut self, peer: PeerId, request: Request) -> Result<Response> {
        ensure!(
            self.saved
                .as_ref()
                .is_some_and(|s| s.forum.network == request.network),
            "论坛不匹配"
        );
        let rate = self.rates.entry(peer).or_insert((Instant::now(), 0));
        if rate.0.elapsed() > Duration::from_secs(10) {
            *rate = (Instant::now(), 0);
        }
        rate.1 += 1;
        ensure!(rate.1 <= 100, "请求过于频繁");
        if let Query::Hello { governance } = &request.query {
            self.apply_governance(governance.clone())?;
        }
        ensure!(self.allowed(&peer), "成员资格无效");
        let s = self.saved.as_ref().unwrap();
        Ok(match request.query {
            Query::Hello { .. } => Response::Hello {
                governance: self.governance(),
            },
            Query::Inventory { after } => {
                let ids: Vec<_> = s
                    .forum
                    .records
                    .keys()
                    .filter(|id| after.is_none_or(|a| **id > a))
                    .take(129)
                    .copied()
                    .collect();
                let more = ids.len() > 128;
                Response::Inventory {
                    ids: ids.into_iter().take(128).collect(),
                    more,
                }
            }
            Query::Records { ids } => {
                ensure!(ids.len() <= 8, "单次记录请求过多");
                Response::Records {
                    records: ids
                        .iter()
                        .filter_map(|id| s.forum.records.get(id).cloned())
                        .collect(),
                }
            }
            Query::Body { hash } => {
                let referenced = s
                    .forum
                    .records
                    .values()
                    .any(|r| matches!(r.record.action,Action::Content{body,..} if body==hash));
                Response::Body {
                    hash,
                    bytes: if referenced {
                        s.forum.body(&hash).map(ToOwned::to_owned)
                    } else {
                        None
                    },
                }
            }
        })
    }
    fn receive(&mut self, pending: Pending, response: Response) -> Result<()> {
        let peer = pending.peer;
        if matches!(response, Response::Denied) {
            anyhow::bail!("对方拒绝论坛访问，请检查批准及撤销状态");
        }
        match (pending.query, response) {
            (Query::Hello { .. }, Response::Hello { governance }) => {
                self.apply_governance(governance)?;
                ensure!(self.allowed(&peer), "对方不是有效成员");
                self.enqueue(peer, Query::Inventory { after: None });
            }
            (Query::Inventory { after }, Response::Inventory { ids, more }) => {
                ensure!(
                    self.allowed(&peer)
                        && ids.len() <= 128
                        && ids.windows(2).all(|p| p[0] < p[1])
                        && ids.first().is_none_or(|id| after.is_none_or(|a| *id > a)),
                    "无效索引响应"
                );
                let s = self.saved.as_ref().unwrap();
                let missing: Vec<_> = ids
                    .iter()
                    .filter(|id| !s.forum.records.contains_key(*id))
                    .copied()
                    .collect();
                for chunk in missing.chunks(8) {
                    self.enqueue(
                        peer,
                        Query::Records {
                            ids: chunk.to_vec(),
                        },
                    );
                }
                if more {
                    if let Some(last) = ids.last() {
                        self.enqueue(peer, Query::Inventory { after: Some(*last) });
                    }
                }
            }
            (Query::Records { ids }, Response::Records { records }) => {
                ensure!(
                    self.allowed(&peer) && records.len() <= ids.len(),
                    "无效记录响应"
                );
                let s = self.saved.as_mut().unwrap();
                for r in records {
                    ensure!(ids.contains(&record_id(&r)?), "收到未请求的记录");
                    s.forum.ingest(r)?;
                }
                let missing = s.forum.missing_dependencies();
                self.refresh();
                for chunk in missing.chunks(8).take(4) {
                    self.enqueue(
                        peer,
                        Query::Records {
                            ids: chunk.to_vec(),
                        },
                    );
                }
            }
            (
                Query::Body { hash },
                Response::Body {
                    hash: received,
                    bytes,
                },
            ) => {
                ensure!(self.allowed(&peer) && hash == received, "无效正文响应");
                if let Some(bytes) = bytes {
                    ensure!(content_id(&bytes) == hash, "正文哈希不匹配");
                    self.saved.as_mut().unwrap().forum.put_body(bytes)?;
                    self.dirty = true;
                    self.status = "正文已校验并缓存".into();
                }
            }
            _ => anyhow::bail!("响应类型不匹配"),
        }
        Ok(())
    }
    async fn run(mut self, mut swarm: super::wire::Swarm, rx: mpsc::Receiver<Command>) {
        let mut ticks = tokio::time::interval(Duration::from_millis(50));
        let mut last_sync = Instant::now();
        let mut last_save = Instant::now();
        loop {
            tokio::select! {
                _=ticks.tick()=>{
                    match rx.try_recv(){Ok(command)=>{let result=self.command(command.request,&mut swarm);let _=command.reply.send(result);},Err(mpsc::TryRecvError::Disconnected)=>break,Err(mpsc::TryRecvError::Empty)=>{}}
                    if last_sync.elapsed()>Duration::from_secs(10){
                        for peer in self.peers.clone(){if !self.requests.values().any(|p|p.peer==peer)&&self.queues.get(&peer).is_none_or(VecDeque::is_empty){self.enqueue(peer,Query::Hello{governance:self.governance()});}}
                        last_sync=Instant::now();
                    }
                    if self.dirty&&last_save.elapsed()>Duration::from_secs(1){if let Err(e)=self.persist(){self.status=format!("保存失败：{e}");}last_save=Instant::now();}
                    self.next(&mut swarm);
                }
                event=swarm.select_next_some()=>match event{
                    SwarmEvent::NewListenAddr{address,..}=>{if campus_address(&address)&&self.listen.len()<8&&!self.listen.contains(&address){self.listen.push(address);}},
                    SwarmEvent::ExpiredListenAddr{address,..}=>{self.listen.retain(|a|a!=&address);},
                    SwarmEvent::ConnectionEstablished{peer_id,..}=>{
                        if self.peers.len()>=8{let _=swarm.disconnect_peer_id(peer_id);}else{self.peers.insert(peer_id);self.enqueue(peer_id,Query::Hello{governance:self.governance()});}
                    }
                    SwarmEvent::ConnectionClosed{peer_id,num_established:0,..}=>{self.peers.remove(&peer_id);self.queues.remove(&peer_id);self.rates.remove(&peer_id);self.requests.retain(|_,p|p.peer!=peer_id);},
                    SwarmEvent::Behaviour(Event::Message{peer,message,..})=>match message{
                        Message::Request{request,channel,..}=>{let answer=self.answer(peer,request).unwrap_or(Response::Denied);let _=swarm.behaviour_mut().send_response(channel,answer);},
                        Message::Response{request_id,response}=>{if let Some(p)=self.requests.remove(&request_id){if p.peer==peer{if let Err(e)=self.receive(p,response){self.status=e.to_string();self.queues.remove(&peer);}}}},
                    },
                    SwarmEvent::Behaviour(Event::OutboundFailure{request_id,error,..})=>{self.requests.remove(&request_id);self.status=format!("同步未完成：{error}");},
                    SwarmEvent::OutgoingConnectionError{error,..}=>{self.status=format!("连接失败：{error}");},
                    _=>{}
                }
            }
        }
        let _ = self.persist();
        self.storage_key.fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state(service: &ForumService) -> Value {
        service.handle(ForumRequest::State).unwrap()["state"].clone()
    }
    fn wait(mut check: impl FnMut() -> bool) {
        let start = Instant::now();
        while !check() {
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "等待论坛同步超时"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    #[test]
    fn two_real_nodes_join_sync_fetch_and_restore_encrypted_state() {
        let a_dir = tempfile::tempdir().unwrap();
        let b_dir = tempfile::tempdir().unwrap();
        let a = ForumService::start([1; 32], [8; 32], a_dir.path().into()).unwrap();
        let b = ForumService::start([2; 32], [9; 32], b_dir.path().into()).unwrap();
        a.handle(ForumRequest::Create {
            name: "校内测试".into(),
        })
        .unwrap();
        let public = state(&b)["own_public"].as_str().unwrap().to_owned();
        let invite = a
            .handle(ForumRequest::Admit {
                member: public,
                inviter: None,
                expires_at: None,
            })
            .unwrap()["code"]
            .as_str()
            .unwrap()
            .to_owned();
        b.handle(ForumRequest::Join { invitation: invite }).unwrap();
        a.handle(ForumRequest::Listen).unwrap();
        let mut connection = String::new();
        wait(|| {
            let v = a.handle(ForumRequest::State).unwrap();
            if let Some(c) = v["code"].as_str() {
                connection = c.into();
                true
            } else {
                false
            }
        });
        b.handle(ForumRequest::Connect { code: connection })
            .unwrap();
        wait(|| state(&a)["peers"].as_u64().unwrap() > 0);
        a.handle(ForumRequest::Publish {
            target: None,
            reply_to: None,
            parents: vec![],
            title: "真实网络".into(),
            body: "中文正文".into(),
        })
        .unwrap();
        b.handle(ForumRequest::Sync).unwrap();
        wait(|| state(&b)["topics"].as_array().unwrap().len() == 1);
        let hash = state(&b)["topics"][0]["versions"][0]["body_hash"]
            .as_str()
            .unwrap()
            .to_owned();
        b.handle(ForumRequest::Fetch { hash }).unwrap();
        wait(|| state(&b)["topics"][0]["versions"][0]["body"] == "中文正文");
        b.handle(ForumRequest::State).unwrap();
        drop(b);
        std::thread::sleep(Duration::from_millis(150));
        let b = ForumService::start([2; 32], [9; 32], b_dir.path().into()).unwrap();
        assert_eq!(state(&b)["topics"][0]["versions"][0]["body"], "中文正文");
    }
    #[test]
    fn identity_of_authenticated_connection_controls_access() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Worker::new(
            Keypair::ed25519_from_bytes([1; 32]).unwrap(),
            [8; 32],
            dir.path().into(),
        )
        .unwrap();
        w.saved = Some(Saved {
            name: "test".into(),
            forum: Forum::new([7; 32], w.key.public().encode_protobuf()).unwrap(),
        });
        w.refresh();
        let stranger = Keypair::ed25519_from_bytes([2; 32])
            .unwrap()
            .public()
            .to_peer_id();
        assert!(w
            .answer(
                stranger,
                Request {
                    network: [7; 32],
                    query: Query::Inventory { after: None }
                }
            )
            .is_err());
        assert!(w
            .answer(
                stranger,
                Request {
                    network: [7; 32],
                    query: Query::Body { hash: [0; 32] }
                }
            )
            .is_err());
        assert!(w
            .answer(
                stranger,
                Request {
                    network: [7; 32],
                    query: Query::Hello { governance: vec![] }
                }
            )
            .is_err());
    }
}
