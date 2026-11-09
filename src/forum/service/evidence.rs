//! Bounded governance proofs include signed content ancestors needed by checkpoints/locks.
use super::*;
const LIMIT: usize = 192 * 1024;
const COUNT: usize = 768;
fn visit(
    id: Digest,
    records: &BTreeMap<Digest, SignedRecord>,
    existing: &BTreeMap<Digest, SignedRecord>,
    seen: &mut BTreeSet<Digest>,
    out: &mut Vec<Digest>,
    depth: usize,
) -> Result<()> {
    ensure!(depth <= 512, "治理证据依赖过深");
    if !seen.insert(id) {
        return Ok(());
    }
    let r = records
        .get(&id)
        .or_else(|| existing.get(&id))
        .ok_or_else(|| anyhow::anyhow!("治理证据缺少依赖"))?;
    ensure!(
        !matches!(r.record.action, Action::Report { .. }),
        "举报不能充当治理授权证据"
    );
    for parent in super::super::dependencies(&r.record.action) {
        visit(parent, records, existing, seen, out, depth + 1)?;
    }
    if records.contains_key(&id) {
        out.push(id);
    }
    Ok(())
}
pub(super) fn bundle(forum: &Forum) -> Result<Vec<SignedRecord>> {
    let mut roots: Vec<_> = forum
        .records
        .iter()
        .filter_map(|(id, r)| match r.record.action {
            Action::Governance { sequence, .. } => Some((sequence, *id)),
            _ => None,
        })
        .collect();
    roots.sort();
    ensure!(roots.len() <= 256, "一期治理记录容量已满");
    let mut seen = BTreeSet::new();
    let mut order = vec![];
    for (_, id) in roots {
        visit(
            id,
            &forum.records,
            &BTreeMap::new(),
            &mut seen,
            &mut order,
            0,
        )?;
        ensure!(order.len() <= COUNT, "治理依赖超过一期证据容量");
    }
    let records: Vec<_> = order
        .into_iter()
        .map(|id| forum.records[&id].clone())
        .collect();
    ensure!(
        postcard::to_allocvec(&records)?.len() <= LIMIT,
        "治理证据超过 192 KiB，需要升级同步格式"
    );
    Ok(records)
}
pub(super) fn ingest(forum: &Forum, records: Vec<SignedRecord>) -> Result<Forum> {
    ensure!(
        records.len() <= COUNT && postcard::to_allocvec(&records)?.len() <= LIMIT,
        "治理证据过大"
    );
    let mut incoming = BTreeMap::new();
    let mut roots = vec![];
    for record in records {
        validate(&record, forum.network)?;
        if let Action::Governance { sequence, .. } = record.record.action {
            ensure!(
                record.record.author == forum.admin && sequence <= 256,
                "治理签发者或序号无效"
            );
            roots.push((sequence, record_id(&record)?));
        } else {
            ensure!(
                matches!(record.record.action, Action::Content { .. }),
                "治理证据类型无效"
            );
        }
        ensure!(
            incoming.insert(record_id(&record)?, record).is_none(),
            "治理证据重复"
        );
    }
    roots.sort();
    let mut seen = BTreeSet::new();
    let mut order = vec![];
    for (_, id) in roots {
        visit(id, &incoming, &forum.records, &mut seen, &mut order, 0)?;
    }
    ensure!(order.len() == incoming.len(), "治理包夹带无关内容");
    let mut candidate = forum.clone();
    for id in order {
        candidate.ingest(incoming.remove(&id).unwrap())?;
    }
    Ok(candidate)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checkpoint_and_lock_evidence_bootstrap_members_without_body() {
        let admin = Keypair::ed25519_from_bytes([1; 32]).unwrap();
        let member = Keypair::ed25519_from_bytes([2; 32]).unwrap();
        let mut source = Forum::new([7; 32], admin.public().encode_protobuf()).unwrap();
        let body = source.put_body(b"not part of invitation".to_vec()).unwrap();
        let root = source
            .ingest(
                sign_record(
                    &admin,
                    [7; 32],
                    Action::Content {
                        target: None,
                        reply_to: None,
                        parents: vec![],
                        title: "proof".into(),
                        body,
                        attachments: vec![],
                    },
                )
                .unwrap(),
            )
            .unwrap();
        for change in [
            GovernanceChange::Checkpoint {
                records: vec![root],
            },
            GovernanceChange::Lock {
                target: root,
                locked: true,
                heads: vec![root],
            },
            GovernanceChange::Admit {
                member: member.public().encode_protobuf(),
                inviter: admin.public().encode_protobuf(),
                expires_at: now() + 3600,
            },
        ] {
            let v = source.rebuild_at(now());
            source
                .ingest(
                    sign_record(
                        &admin,
                        [7; 32],
                        Action::Governance {
                            sequence: v.governance_sequence + 1,
                            previous: v.governance_head,
                            change,
                        },
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let empty = Forum::new([7; 32], admin.public().encode_protobuf()).unwrap();
        let imported = ingest(&empty, bundle(&source).unwrap()).unwrap();
        assert!(imported.can_access(&member.public().encode_protobuf(), now()));
        assert!(imported.body(&body).is_none());
        assert_eq!(imported.rebuild_at(now()).governance_sequence, 3);
        let mut bad = bundle(&source).unwrap();
        bad.push(
            sign_record(
                &member,
                [7; 32],
                Action::Content {
                    target: None,
                    reply_to: None,
                    parents: vec![],
                    title: "unrelated".into(),
                    body,
                    attachments: vec![],
                },
            )
            .unwrap(),
        );
        assert!(ingest(&empty, bad).is_err());
    }
}
