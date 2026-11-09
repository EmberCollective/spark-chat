//! Verified append-only forum state. No sockets, private keys or source IPs are stored.
use crate::protocol::forum::*;
use anyhow::{ensure, Result};
use libp2p_identity::{Keypair, PublicKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};

mod merge;
pub mod service;
mod storage;
#[cfg(test)]
mod tests;
mod wire;

const DOMAIN: &[u8] = b"SparkNet/forum/record/v1\0";
const MAX_RECORDS: usize = 50_000;
const MAX_RECORD_STORAGE: usize = 32 * 1024 * 1024;
const MAX_BODY_STORAGE: usize = 64 * 1024 * 1024;
const MAX_PENDING_RECORDS: usize = 512;
const MAX_PENDING_STORAGE: usize = 256 * 1024;
const MAX_PARENTS: usize = 8;
const MAX_DEPTH: usize = 256;

pub fn content_id(bytes: &[u8]) -> Digest {
    Sha256::digest(bytes).into()
}
pub fn record_id(record: &SignedRecord) -> Result<Digest> {
    Ok(content_id(&postcard::to_allocvec(record)?))
}
fn record_cost(record: &SignedRecord) -> Result<usize> {
    Ok(postcard::to_allocvec(record)?.len() + 32 + 10)
}
fn signing_bytes(record: &ForumRecord) -> Result<Vec<u8>> {
    let mut bytes = DOMAIN.to_vec();
    bytes.extend(postcard::to_allocvec(record)?);
    Ok(bytes)
}
fn public(bytes: &[u8]) -> Result<PublicKey> {
    ensure!(bytes.len() <= 64, "公钥过长");
    let key = PublicKey::try_decode_protobuf(bytes)?;
    ensure!(
        key.clone().try_into_ed25519().is_ok(),
        "论坛仅支持 Ed25519 身份"
    );
    ensure!(key.encode_protobuf() == bytes, "公钥编码不规范");
    Ok(key)
}

pub fn sign_record(key: &Keypair, network: Digest, action: Action) -> Result<SignedRecord> {
    let mut nonce = [0; 16];
    rand::thread_rng().fill_bytes(&mut nonce);
    let record = ForumRecord {
        network,
        author: key.public().encode_protobuf(),
        nonce,
        action,
    };
    let signature = key.sign(&signing_bytes(&record)?)?;
    let signed = SignedRecord { record, signature };
    validate(&signed, network)?;
    Ok(signed)
}

fn validate(signed: &SignedRecord, network: Digest) -> Result<()> {
    ensure!(signed.record.network == network, "论坛网络不匹配");
    ensure!(
        postcard::to_allocvec(signed)?.len() <= MAX_RECORD_BYTES,
        "论坛记录过大"
    );
    ensure!(signed.signature.len() == 64, "签名长度不合法");
    ensure!(
        public(&signed.record.author)?.verify(&signing_bytes(&signed.record)?, &signed.signature),
        "论坛签名无效"
    );
    match &signed.record.action {
        Action::Content {
            target,
            reply_to,
            parents,
            title,
            attachments,
            ..
        } => {
            ensure!(title.chars().count() <= 100, "标题过长");
            ensure!(parents.len() <= MAX_PARENTS, "父版本过多");
            ensure!(
                parents.windows(2).all(|p| p[0] < p[1]),
                "父版本必须唯一且排序"
            );
            ensure!(
                target.is_some() == !parents.is_empty(),
                "编辑必须引用父版本"
            );
            ensure!(
                target.is_none() || reply_to.is_none(),
                "编辑不能更换回复目标"
            );
            ensure!(attachments.len() <= 5, "附件过多");
            let mut total = 0u64;
            let mut seen = BTreeSet::new();
            for item in attachments {
                ensure!(
                    !item.name.is_empty()
                        && item.name.len() <= 255
                        && !item.name.chars().any(char::is_control),
                    "附件名称无效"
                );
                ensure!(
                    item.size <= MAX_ATTACHMENT_BYTES && seen.insert(item.hash),
                    "附件大小或重复引用无效"
                );
                total = total
                    .checked_add(item.size)
                    .ok_or_else(|| anyhow::anyhow!("附件大小溢出"))?;
            }
            ensure!(total <= MAX_ATTACHMENTS_BYTES, "附件合计超过 30 MB");
        }
        Action::Governance {
            sequence,
            previous,
            change,
        } => {
            ensure!(
                *sequence > 0 && (*sequence == 1) == previous.is_none(),
                "治理序号不合法"
            );
            match change {
                GovernanceChange::Admit {
                    member,
                    inviter,
                    expires_at,
                } => {
                    public(member)?;
                    public(inviter)?;
                    ensure!(*expires_at > 0, "成员有效期无效");
                }
                GovernanceChange::Revoke { member } => {
                    public(member)?;
                }
                GovernanceChange::Checkpoint { records } => {
                    ensure!(
                        !records.is_empty()
                            && records.len() <= 512
                            && records.windows(2).all(|p| p[0] < p[1]),
                        "检查点必须有序且不重复"
                    );
                }
                GovernanceChange::Moderate { .. } => {}
                GovernanceChange::Category { id, title } => {
                    ensure!(
                        !id.is_empty()
                            && id.len() <= 32
                            && id
                                .bytes()
                                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                        "分类编号无效"
                    );
                    ensure!(
                        !title.trim().is_empty()
                            && title.chars().count() <= 24
                            && !title.chars().any(char::is_control),
                        "分类名称无效"
                    );
                }
                GovernanceChange::AssignCategory { category, .. } => {
                    ensure!(!category.is_empty() && category.len() <= 32, "分类编号无效");
                }
                GovernanceChange::Lock { locked, heads, .. } => {
                    ensure!(
                        heads.len() <= 512
                            && heads.windows(2).all(|p| p[0] < p[1])
                            && (*locked != heads.is_empty()),
                        "锁定必须引用有序的当前版本，解锁不携带版本"
                    );
                }
            }
        }
        Action::Report { reason, .. } => {
            ensure!(
                !reason.trim().is_empty() && reason.chars().count() <= 500,
                "举报理由须为 1 至 500 字"
            );
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Member {
    pub inviter: Vec<u8>,
    pub expires_at: u64,
    pub revoked: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Topic {
    pub root: Digest,
    pub reply_to: Option<Digest>,
    pub heads: Vec<Digest>,
    pub hidden: bool,
    pub locked: bool,
    pub category: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub id: Digest,
    pub target: Digest,
    pub author: Vec<u8>,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ForumView {
    pub governance_head: Option<Digest>,
    pub governance_sequence: u64,
    pub governance_conflict: bool,
    pub members: BTreeMap<Vec<u8>, Member>,
    pub topics: Vec<Topic>,
    pub categories: BTreeMap<String, String>,
    pub reports: Vec<Report>,
    pub pending: Vec<Digest>,
    pub disputed: Vec<Digest>,
    pub invalid: Vec<Digest>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Forum {
    network: Digest,
    admin: Vec<u8>,
    records: BTreeMap<Digest, SignedRecord>,
    bodies: BTreeMap<Digest, Vec<u8>>,
    // Recomputed from verified records on load, never trusted from the snapshot.
    #[serde(skip)]
    record_storage: usize,
    #[serde(skip)]
    body_storage: usize,
}

impl Forum {
    /// admin must come from the trusted invitation/genesis, never from an arbitrary peer.
    pub fn new(network: Digest, admin: Vec<u8>) -> Result<Self> {
        public(&admin)?;
        Ok(Self {
            network,
            admin,
            records: BTreeMap::new(),
            bodies: BTreeMap::new(),
            record_storage: 0,
            body_storage: 0,
        })
    }
    pub fn network(&self) -> Digest {
        self.network
    }
    pub fn admin(&self) -> &[u8] {
        &self.admin
    }
    pub fn records(&self) -> &BTreeMap<Digest, SignedRecord> {
        &self.records
    }
    pub fn body(&self, hash: &Digest) -> Option<&[u8]> {
        self.bodies.get(hash).map(Vec::as_slice)
    }
    pub fn put_body(&mut self, bytes: Vec<u8>) -> Result<Digest> {
        ensure!(bytes.len() <= MAX_BODY_BYTES, "正文过大");
        let text = std::str::from_utf8(&bytes)?;
        ensure!(text.chars().count() <= 20_000, "正文超过 20000 字");
        let hash = content_id(&bytes);
        if self.bodies.contains_key(&hash) {
            return Ok(hash);
        }
        // Include map keys and length framing so small objects cannot bypass the byte budget.
        let cost = bytes.len() + 32 + 10;
        ensure!(
            self.bodies.len() < MAX_RECORDS && self.body_storage + cost <= MAX_BODY_STORAGE,
            "正文缓存已满"
        );
        self.body_storage += cost;
        self.bodies.insert(hash, bytes);
        Ok(hash)
    }
    pub fn ingest(&mut self, signed: SignedRecord) -> Result<Digest> {
        let id = record_id(&signed)?;
        if self.records.contains_key(&id) {
            return Ok(id);
        }
        self.insert_verified(signed)?;
        // Insert before classification: a missing parent or admission must be allowed to
        // resolve an already full pending set rather than being rejected at its entrance.
        if let Err(error) = self.check_pending_budget() {
            let removed = self.records.remove(&id).expect("inserted record");
            self.record_storage -= record_cost(&removed)?;
            return Err(error);
        }
        Ok(id)
    }

    fn insert_verified(&mut self, signed: SignedRecord) -> Result<Digest> {
        validate(&signed, self.network)?;
        if matches!(signed.record.action, Action::Governance { .. }) {
            ensure!(
                signed.record.author == self.admin,
                "只有网络管理员能签署治理操作"
            );
        }
        let id = record_id(&signed)?;
        if !self.records.contains_key(&id) {
            let cost = record_cost(&signed)?;
            ensure!(
                self.records.len() < MAX_RECORDS
                    && self.record_storage + cost <= MAX_RECORD_STORAGE,
                "论坛记录容量已满"
            );
            self.record_storage += cost;
            self.records.insert(id, signed);
        }
        Ok(id)
    }

    fn check_pending_budget(&self) -> Result<()> {
        let pending = self.rebuild().pending;
        ensure!(
            pending.len() <= MAX_PENDING_RECORDS,
            "待验证记录过多，请先补齐依赖"
        );
        let bytes = pending.iter().try_fold(0usize, |sum, id| -> Result<_> {
            Ok(sum + record_cost(&self.records[id])?)
        })?;
        ensure!(
            bytes <= MAX_PENDING_STORAGE,
            "待验证记录空间已满，请先补齐依赖"
        );
        Ok(())
    }

    pub fn can_access(&self, author: &[u8], now: u64) -> bool {
        let view = self.rebuild();
        !view.governance_conflict
            && view
                .members
                .get(author)
                .is_some_and(|m| !m.revoked && m.expires_at > now)
    }
    pub fn missing_dependencies(&self) -> Vec<Digest> {
        let mut missing = BTreeSet::new();
        for record in self.records.values() {
            for id in dependencies(&record.record.action) {
                if !self.records.contains_key(&id) {
                    missing.insert(id);
                }
            }
        }
        missing.into_iter().take(512).collect()
    }

    pub fn rebuild(&self) -> ForumView {
        let mut view = ForumView::default();
        view.members.insert(
            self.admin.clone(),
            Member {
                inviter: self.admin.clone(),
                expires_at: u64::MAX,
                revoked: false,
            },
        );
        let mut confirmed = BTreeSet::new();
        let mut hidden = BTreeMap::new();
        view.categories.insert("general".into(), "综合".into());
        let mut categories = BTreeMap::new();
        let mut locks: BTreeMap<Digest, BTreeSet<Digest>> = BTreeMap::new();
        let mut applied = BTreeSet::new();
        loop {
            let candidates: Vec<_> = self.records.iter().filter(|(_, r)| matches!(&r.record.action,
                Action::Governance { sequence, previous, .. } if *sequence == view.governance_sequence + 1 && *previous == view.governance_head)).collect();
            if candidates.len() > 1 {
                view.governance_conflict = true;
                break;
            }
            let Some((id, record)) = candidates.first() else {
                break;
            };
            let Action::Governance { change, .. } = &record.record.action else {
                unreachable!()
            };
            let valid = match change {
                GovernanceChange::Admit {
                    member,
                    inviter,
                    expires_at,
                } => {
                    // The first inviter is immutable: renewal/re-admission may extend
                    // access but cannot rewrite provenance or point back to a descendant.
                    if !view.members.get(inviter).is_some_and(|m| !m.revoked)
                        || member == inviter
                        || view
                            .members
                            .get(member)
                            .is_some_and(|m| &m.inviter != inviter)
                        || member == &self.admin
                    {
                        false
                    } else {
                        view.members.insert(
                            member.clone(),
                            Member {
                                inviter: inviter.clone(),
                                expires_at: *expires_at,
                                revoked: false,
                            },
                        );
                        true
                    }
                }
                GovernanceChange::Revoke { member } => {
                    if member == &self.admin {
                        false
                    } else if let Some(m) = view.members.get_mut(member) {
                        m.revoked = true;
                        true
                    } else {
                        false
                    }
                }
                GovernanceChange::Checkpoint { records } => {
                    let mut all = BTreeSet::new();
                    let ok = records
                        .iter()
                        .all(|id| self.collect_valid(*id, &view.members, &mut all, 0));
                    if ok {
                        confirmed.extend(all);
                    }
                    ok
                }
                GovernanceChange::Category { id, title } => {
                    view.categories.insert(id.clone(), title.clone());
                    true
                }
                GovernanceChange::AssignCategory { target, category } => {
                    if view.categories.contains_key(category)
                        && self.records.get(target).is_some_and(|r| {
                            matches!(
                                r.record.action,
                                Action::Content {
                                    target: None,
                                    reply_to: None,
                                    ..
                                }
                            )
                        })
                    {
                        categories.insert(*target, category.clone());
                        true
                    } else {
                        false
                    }
                }
                GovernanceChange::Lock {
                    target,
                    locked,
                    heads,
                } => {
                    let mut accepted = BTreeSet::new();
                    let ok = self.records.get(target).is_some_and(|r| matches!(r.record.action, Action::Content { target: None, reply_to: None, .. }))
                        && heads.iter().all(|id| self.records.get(id).is_some_and(|r| matches!(r.record.action, Action::Content { target: t, reply_to, .. } if t.unwrap_or(*id) == *target || reply_to == Some(*target) || t.is_some_and(|root| self.records.get(&root).is_some_and(|root_record| matches!(root_record.record.action, Action::Content { reply_to: Some(topic), .. } if topic == *target))))) && self.collect_valid(*id, &view.members, &mut accepted, 0));
                    if ok {
                        if *locked {
                            locks.insert(*target, accepted);
                        } else {
                            locks.remove(target);
                        }
                    }
                    ok
                }
                GovernanceChange::Moderate {
                    target,
                    hidden: value,
                } => {
                    if self.records.get(target).is_some_and(|r| {
                        matches!(r.record.action, Action::Content { target: None, .. })
                    }) {
                        hidden.insert(*target, *value);
                        true
                    } else {
                        false
                    }
                }
            };
            if !valid {
                view.pending.push(**id);
                break;
            }
            view.governance_sequence += 1;
            view.governance_head = Some(**id);
            applied.insert(**id);
        }
        let mut valid = BTreeMap::<Digest, (Digest, usize)>::new();
        let mut disputed = BTreeSet::new();
        // Bounded passes make arrival order irrelevant without recursive untrusted traversal.
        for _ in 0..=MAX_DEPTH {
            let before = valid.len();
            for (id, signed) in &self.records {
                let Action::Content {
                    target,
                    reply_to,
                    parents,
                    ..
                } = &signed.record.action
                else {
                    continue;
                };
                if valid.contains_key(id) || !view.members.contains_key(&signed.record.author) {
                    continue;
                }
                let root = target.unwrap_or(*id);
                let mut depth = 0;
                if let Some(target) = target {
                    if !self.records.get(target).is_some_and(|r| {
                        matches!(r.record.action, Action::Content { target: None, .. })
                    }) {
                        continue;
                    }
                    if !parents.iter().all(|p| {
                        valid.get(p).is_some_and(|(r, d)| {
                            depth = depth.max(*d + 1);
                            r == target
                        })
                    }) {
                        continue;
                    }
                } else if let Some(reply) = reply_to {
                    if !valid.get(reply).is_some_and(|(r, _)| r == reply)
                        || !self.records.get(reply).is_some_and(|r| {
                            matches!(r.record.action, Action::Content { reply_to: None, .. })
                        })
                    {
                        continue;
                    }
                    depth = valid[reply].1 + 1;
                }
                if depth > MAX_DEPTH {
                    continue;
                }
                let parent_topic = match &self.records[&root].record.action {
                    Action::Content { reply_to, .. } => *reply_to,
                    _ => None,
                };
                let locked_out = locks
                    .get(&root)
                    .is_some_and(|accepted| !accepted.contains(id))
                    || parent_topic.is_some_and(|topic| {
                        locks
                            .get(&topic)
                            .is_some_and(|accepted| !accepted.contains(id))
                    });
                if locked_out
                    || view.governance_conflict
                    || (view.members[&signed.record.author].revoked && !confirmed.contains(id))
                    || parents.iter().any(|p| disputed.contains(p))
                    || reply_to.is_some_and(|r| disputed.contains(&r))
                {
                    disputed.insert(*id);
                }
                valid.insert(*id, (root, depth));
            }
            if valid.len() == before {
                break;
            }
        }
        let mut topics = BTreeMap::<Digest, Topic>::new();
        for (id, (root, _)) in &valid {
            if disputed.contains(id) {
                continue;
            }
            let topic = topics.entry(*root).or_insert_with(|| {
                let reply_to = match &self.records[root].record.action {
                    Action::Content { reply_to, .. } => *reply_to,
                    _ => None,
                };
                Topic {
                    root: *root,
                    reply_to,
                    heads: Vec::new(),
                    locked: locks.contains_key(root)
                        || reply_to.is_some_and(|r| locks.contains_key(&r)),
                    category: categories
                        .get(&reply_to.unwrap_or(*root))
                        .cloned()
                        .unwrap_or_else(|| "general".into()),
                    hidden: hidden.get(root).copied().unwrap_or(false)
                        || reply_to.is_some_and(|r| hidden.get(&r).copied().unwrap_or(false)),
                }
            });
            topic.heads.push(*id);
        }
        for topic in topics.values_mut() {
            let consumed: BTreeSet<_> = topic
                .heads
                .iter()
                .flat_map(|id| match &self.records[id].record.action {
                    Action::Content { parents, .. } => parents.clone(),
                    _ => vec![],
                })
                .collect();
            topic.heads.retain(|id| !consumed.contains(id));
        }
        for (id, signed) in &self.records {
            match signed.record.action {
                Action::Report { target, ref reason } => {
                    if !valid.contains_key(&target)
                        || !view.members.contains_key(&signed.record.author)
                    {
                        view.pending.push(*id);
                    } else if view.members[&signed.record.author].revoked
                        || view.governance_conflict
                    {
                        disputed.insert(*id);
                    } else {
                        view.reports.push(Report {
                            id: *id,
                            target,
                            author: signed.record.author.clone(),
                            reason: reason.clone(),
                        });
                    }
                }
                Action::Content { .. } if !valid.contains_key(id) => view.pending.push(*id),
                Action::Governance { .. } if !applied.contains(id) => view.pending.push(*id),
                _ => {}
            }
        }
        view.pending.sort();
        view.pending.dedup();
        view.disputed = disputed.into_iter().collect();
        view.topics = topics.into_values().collect();
        view
    }

    fn collect_valid(
        &self,
        id: Digest,
        members: &BTreeMap<Vec<u8>, Member>,
        out: &mut BTreeSet<Digest>,
        depth: usize,
    ) -> bool {
        if depth > MAX_DEPTH {
            return false;
        }
        if out.contains(&id) {
            return true;
        }
        let Some(record) = self.records.get(&id) else {
            return false;
        };
        let Action::Content {
            target,
            parents,
            reply_to,
            ..
        } = &record.record.action
        else {
            return false;
        };
        if !members.contains_key(&record.record.author) {
            return false;
        }
        if let Some(root) = target {
            if !self
                .records
                .get(root)
                .is_some_and(|r| matches!(r.record.action, Action::Content { target: None, .. }))
            {
                return false;
            }
            for parent in parents {
                if !self
                    .records
                    .get(parent)
                    .is_some_and(|r| match &r.record.action {
                        Action::Content { target, .. } => target.unwrap_or(*parent) == *root,
                        _ => false,
                    })
                {
                    return false;
                }
            }
        }
        if let Some(reply) = reply_to {
            if !self.records.get(reply).is_some_and(|r| {
                matches!(
                    r.record.action,
                    Action::Content {
                        target: None,
                        reply_to: None,
                        ..
                    }
                )
            }) {
                return false;
            }
        }
        if !dependencies(&record.record.action)
            .iter()
            .all(|p| self.collect_valid(*p, members, out, depth + 1))
        {
            return false;
        }
        out.insert(id);
        true
    }
}

fn dependencies(action: &Action) -> Vec<Digest> {
    match action {
        Action::Report { target, .. } => vec![*target],
        Action::Content {
            target,
            reply_to,
            parents,
            ..
        } => parents
            .iter()
            .copied()
            .chain(*target)
            .chain(*reply_to)
            .collect(),
        Action::Governance {
            previous, change, ..
        } => previous
            .iter()
            .copied()
            .chain(match change {
                GovernanceChange::Checkpoint { records } => records.clone(),
                GovernanceChange::Moderate { target, .. }
                | GovernanceChange::AssignCategory { target, .. } => vec![*target],
                GovernanceChange::Lock { target, heads, .. } => std::iter::once(*target)
                    .chain(heads.iter().copied())
                    .collect(),
                _ => vec![],
            })
            .collect(),
    }
}
