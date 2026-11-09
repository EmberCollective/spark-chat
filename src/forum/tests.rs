use super::*;
fn key(n: u8) -> Keypair {
    Keypair::ed25519_from_bytes([n; 32]).unwrap()
}
fn pk(n: u8) -> Vec<u8> {
    key(n).public().encode_protobuf()
}
fn gov(f: &Forum, change: GovernanceChange) -> SignedRecord {
    let v = f.rebuild();
    sign_record(
        &key(1),
        f.network(),
        Action::Governance {
            sequence: v.governance_sequence + 1,
            previous: v.governance_head,
            change,
        },
    )
    .unwrap()
}
fn fixture() -> Forum {
    let mut f = Forum::new([9; 32], pk(1)).unwrap();
    for n in 2..=6 {
        let r = gov(
            &f,
            GovernanceChange::Admit {
                member: pk(n),
                inviter: pk(1),
                expires_at: 1000,
            },
        );
        f.ingest(r).unwrap();
    }
    f
}
fn post(
    f: &mut Forum,
    author: u8,
    target: Option<Digest>,
    mut parents: Vec<Digest>,
    text: &str,
) -> SignedRecord {
    parents.sort();
    let body = f.put_body(text.as_bytes().to_vec()).unwrap();
    sign_record(
        &key(author),
        f.network(),
        Action::Content {
            target,
            reply_to: None,
            parents,
            title: "主题".into(),
            body,
            attachments: vec![],
        },
    )
    .unwrap()
}
#[test]
fn tamper_network_and_non_admin_revocation_are_rejected() {
    let mut f = fixture();
    let mut r = post(&mut f, 2, None, vec![], "原文");
    r.record.nonce[0] ^= 1;
    assert!(f.ingest(r).is_err());
    let r = sign_record(
        &key(2),
        f.network(),
        Action::Governance {
            sequence: 6,
            previous: f.rebuild().governance_head,
            change: GovernanceChange::Revoke { member: pk(3) },
        },
    )
    .unwrap();
    assert!(f.ingest(r).is_err());
    let r = sign_record(
        &key(1),
        [8; 32],
        Action::Governance {
            sequence: 1,
            previous: None,
            change: GovernanceChange::Revoke { member: pk(2) },
        },
    )
    .unwrap();
    assert!(f.ingest(r).is_err());
}
#[test]
fn three_partitions_converge_with_missing_parents_duplicates_and_resolution() {
    let mut f = fixture();
    let original = post(&mut f, 2, None, vec![], "原文");
    let root = f.ingest(original.clone()).unwrap();
    let mut partitions = [f.clone(), f.clone(), f.clone()];
    let mut updates = vec![];
    for (i, p) in partitions.iter_mut().enumerate() {
        let r = post(p, i as u8 + 3, Some(root), vec![root], &format!("修改{i}"));
        p.ingest(r.clone()).unwrap();
        updates.push(r);
    }
    let mut late = fixture();
    for r in updates.iter().rev() {
        late.ingest(r.clone()).unwrap();
    }
    assert_eq!(late.missing_dependencies(), vec![root]);
    late.ingest(original).unwrap();
    for (i, p) in partitions.iter_mut().enumerate() {
        for r in updates.iter().cycle().skip(i).take(6) {
            p.ingest(r.clone()).unwrap();
        }
        assert_eq!(p.rebuild().topics, late.rebuild().topics);
    }
    let heads = late.rebuild().topics[0].heads.clone();
    assert_eq!(heads.len(), 3);
    let resolved = post(&mut late, 6, Some(root), heads, "共同解决");
    let resolved_id = late.ingest(resolved.clone()).unwrap();
    for p in &mut partitions {
        p.ingest(resolved.clone()).unwrap();
        assert_eq!(p.rebuild().topics[0].heads, vec![resolved_id]);
    }
}
#[test]
fn checkpoint_keeps_history_and_revocation_quarantines_unconfirmed_descendants() {
    let mut f = fixture();
    let original = post(&mut f, 2, None, vec![], "原文");
    let root = f.ingest(original).unwrap();
    let checkpoint = gov(
        &f,
        GovernanceChange::Checkpoint {
            records: vec![root],
        },
    );
    f.ingest(checkpoint).unwrap();
    let edit = post(&mut f, 2, Some(root), vec![root], "离线编辑");
    let edit_id = f.ingest(edit).unwrap();
    let follow = post(&mut f, 3, Some(root), vec![edit_id], "依赖编辑");
    let follow_id = f.ingest(follow).unwrap();
    let revoke = gov(&f, GovernanceChange::Revoke { member: pk(2) });
    f.ingest(revoke).unwrap();
    let view = f.rebuild();
    assert_eq!(view.topics[0].heads, vec![root]);
    assert!(view.disputed.contains(&edit_id) && view.disputed.contains(&follow_id));
    assert!(!f.can_access(&pk(2), 10));
    assert!(f.can_access(&pk(3), 10));
    assert!(!f.can_access(&pk(3), 1000));
    let checkpoint = gov(
        &f,
        GovernanceChange::Checkpoint {
            records: vec![follow_id],
        },
    );
    f.ingest(checkpoint).unwrap();
    assert_eq!(f.rebuild().topics[0].heads, vec![follow_id]);
}
#[test]
fn governance_fork_fails_closed_and_cannot_revoke_admin() {
    let mut f = fixture();
    let a = gov(&f, GovernanceChange::Revoke { member: pk(2) });
    let b = gov(&f, GovernanceChange::Revoke { member: pk(3) });
    f.ingest(a).unwrap();
    f.ingest(b).unwrap();
    assert!(f.rebuild().governance_conflict);
    assert!(!f.can_access(&pk(4), 1));
    let mut f = fixture();
    let a = gov(&f, GovernanceChange::Revoke { member: pk(1) });
    f.ingest(a).unwrap();
    assert!(!f.rebuild().members[&pk(1)].revoked);
}
#[test]
fn ciphertext_roundtrip_revalidates_trust_and_detects_corruption() {
    let mut f = fixture();
    let r = post(&mut f, 2, None, vec![], "不能以明文落盘");
    f.ingest(r).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("forum.bin");
    f.save(&path, &[7; 32]).unwrap();
    let mut data = std::fs::read(&path).unwrap();
    assert!(!data
        .windows("不能以明文落盘".len())
        .any(|s| s == "不能以明文落盘".as_bytes()));
    let restored = Forum::load(&path, &[7; 32], f.network(), f.admin()).unwrap();
    assert_eq!(restored.rebuild().topics, f.rebuild().topics);
    assert!(Forum::load(&path, &[7; 32], f.network(), &pk(2)).is_err());
    assert!(Forum::load(&path, &[6; 32], f.network(), f.admin()).is_err());
    let last = data.len() - 1;
    data[last] ^= 1;
    std::fs::write(&path, data).unwrap();
    assert!(Forum::load(&path, &[7; 32], f.network(), f.admin()).is_err());
}

#[test]
fn record_and_body_byte_limits_reject_atomically_and_duplicates_are_free() {
    let mut f = fixture();
    let record = post(&mut f, 2, None, vec![], "budget");
    let cost = record_cost(&record).unwrap();
    f.record_storage = MAX_RECORD_STORAGE - cost;
    let id = f.ingest(record.clone()).unwrap();
    assert_eq!(f.record_storage, MAX_RECORD_STORAGE);
    assert_eq!(f.ingest(record).unwrap(), id);
    let extra = post(&mut f, 2, None, vec![], "budget");
    let before = f.records.len();
    assert!(f.ingest(extra).is_err());
    assert_eq!(f.records.len(), before);
    assert_eq!(f.record_storage, MAX_RECORD_STORAGE);

    let body = b"different body".to_vec();
    f.body_storage = MAX_BODY_STORAGE - body.len() - 42;
    let hash = f.put_body(body.clone()).unwrap();
    assert_eq!(f.body_storage, MAX_BODY_STORAGE);
    assert_eq!(f.put_body(body).unwrap(), hash);
    assert!(f.put_body(b"overflow".to_vec()).is_err());
    assert_eq!(f.body_storage, MAX_BODY_STORAGE);
}

#[test]
fn pending_count_limit_still_accepts_the_missing_parent_and_snapshot_reload() {
    let mut f = fixture();
    let original = post(&mut f, 2, None, vec![], "base");
    let root = record_id(&original).unwrap();
    for _ in 0..MAX_PENDING_RECORDS {
        let record = post(&mut f, 3, Some(root), vec![root], "edit");
        f.ingest(record).unwrap();
    }
    assert_eq!(f.rebuild().pending.len(), MAX_PENDING_RECORDS);
    let before = f.record_storage;
    let extra = post(&mut f, 3, Some(root), vec![root], "excess");
    assert!(f.ingest(extra).is_err());
    assert_eq!(f.record_storage, before);
    assert_eq!(f.rebuild().pending.len(), MAX_PENDING_RECORDS);
    f.ingest(original).unwrap();
    assert!(f.rebuild().pending.is_empty());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("snapshot");
    f.save(&path, &[5; 32]).unwrap();
    let loaded = Forum::load(&path, &[5; 32], f.network, f.admin()).unwrap();
    assert_eq!(loaded.rebuild().topics, f.rebuild().topics);
    assert_eq!(loaded.record_storage, f.record_storage);
    assert_eq!(loaded.body_storage, f.body_storage);
}

#[test]
fn pending_bytes_stop_large_unknown_author_records_without_blocking_members() {
    let mut f = fixture();
    let mut rejected = false;
    for _ in 0..MAX_PENDING_RECORDS {
        let mut record = post(&mut f, 7, None, vec![], "unknown author");
        if let Action::Content {
            attachments, title, ..
        } = &mut record.record.action
        {
            *title = "文".repeat(100);
            *attachments = (0..5)
                .map(|i| Attachment {
                    name: "x".repeat(255),
                    hash: [i; 32],
                    size: 1,
                })
                .collect();
        }
        record.signature = key(7)
            .sign(&signing_bytes(&record.record).unwrap())
            .unwrap();
        let before = f.record_storage;
        if f.ingest(record).is_err() {
            assert_eq!(f.record_storage, before);
            rejected = true;
            break;
        }
    }
    assert!(rejected);
    assert!(f.rebuild().pending.len() < MAX_PENDING_RECORDS);
    let valid = post(&mut f, 2, None, vec![], "valid content still works");
    let id = f.ingest(valid).unwrap();
    assert!(!f.rebuild().pending.contains(&id));
}

#[test]
fn categories_reports_and_lock_converge_without_clocks() {
    let mut f = fixture();
    let original = post(&mut f, 2, None, vec![], "原文");
    let root = f.ingest(original).unwrap();
    let reply_body = f.put_body(b"reply".to_vec()).unwrap();
    let reply = f
        .ingest(
            sign_record(
                &key(3),
                f.network(),
                Action::Content {
                    target: None,
                    reply_to: Some(root),
                    parents: vec![],
                    title: String::new(),
                    body: reply_body,
                    attachments: vec![],
                },
            )
            .unwrap(),
        )
        .unwrap();
    let category = gov(
        &f,
        GovernanceChange::Category {
            id: "study".into(),
            title: "学习".into(),
        },
    );
    f.ingest(category).unwrap();
    let assign = gov(
        &f,
        GovernanceChange::AssignCategory {
            target: root,
            category: "study".into(),
        },
    );
    f.ingest(assign).unwrap();
    let report = sign_record(
        &key(3),
        f.network(),
        Action::Report {
            target: root,
            reason: "需管理员核查".into(),
        },
    )
    .unwrap();
    let report_id = f.ingest(report).unwrap();
    let view = f.rebuild();
    assert_eq!(view.reports[0].id, report_id);
    assert!(!view.members[&pk(2)].revoked);
    assert_eq!(
        view.topics
            .iter()
            .find(|t| t.root == root)
            .unwrap()
            .category,
        "study"
    );
    let concurrent = post(&mut f, 2, Some(root), vec![root], "分区新增修改");
    let concurrent_id = record_id(&concurrent).unwrap();
    let mut heads = vec![root, reply];
    heads.sort();
    let lock = gov(
        &f,
        GovernanceChange::Lock {
            target: root,
            locked: true,
            heads,
        },
    );
    let mut a = f.clone();
    let mut b = f.clone();
    a.ingest(concurrent.clone()).unwrap();
    a.ingest(lock.clone()).unwrap();
    b.ingest(lock).unwrap();
    b.ingest(concurrent).unwrap();
    assert_eq!(a.rebuild().topics, b.rebuild().topics);
    let view = a.rebuild();
    assert!(view.disputed.contains(&concurrent_id));
    assert_eq!(view.topics.len(), 2, "已有回复保留");
    assert!(view.topics.iter().all(|t| t.locked));
    assert_eq!(
        view.topics.iter().find(|t| t.root == root).unwrap().heads,
        vec![root]
    );
    let new_reply = a
        .ingest(
            sign_record(
                &key(3),
                a.network(),
                Action::Content {
                    target: None,
                    reply_to: Some(root),
                    parents: vec![],
                    title: String::new(),
                    body: reply_body,
                    attachments: vec![],
                },
            )
            .unwrap(),
        )
        .unwrap();
    assert!(a.rebuild().disputed.contains(&new_reply));
    let unlock = gov(
        &a,
        GovernanceChange::Lock {
            target: root,
            locked: false,
            heads: vec![],
        },
    );
    a.ingest(unlock).unwrap();
    assert!(!a.rebuild().topics.iter().any(|t| t.locked));
    assert!(!a.rebuild().disputed.contains(&concurrent_id));
}

#[test]
fn report_and_category_permissions_reject_forgery_and_unknown_targets() {
    let mut f = fixture();
    let forged = sign_record(
        &key(2),
        f.network(),
        Action::Governance {
            sequence: 6,
            previous: f.rebuild().governance_head,
            change: GovernanceChange::Category {
                id: "evil".into(),
                title: "越权".into(),
            },
        },
    )
    .unwrap();
    assert!(f.ingest(forged).is_err());
    let unknown = sign_record(
        &key(2),
        f.network(),
        Action::Report {
            target: [42; 32],
            reason: "待补目标".into(),
        },
    )
    .unwrap();
    let id = f.ingest(unknown).unwrap();
    assert!(f.rebuild().pending.contains(&id));
    assert!(f.rebuild().reports.is_empty());
    assert!(sign_record(
        &key(2),
        f.network(),
        Action::Report {
            target: [42; 32],
            reason: String::new()
        }
    )
    .is_err());
}

#[test]
fn admission_renewal_preserves_first_inviter_and_prevents_cycles() {
    let mut f = fixture();
    let child = gov(
        &f,
        GovernanceChange::Admit {
            member: pk(7),
            inviter: pk(2),
            expires_at: 1000,
        },
    );
    f.ingest(child).unwrap();
    let renew = gov(
        &f,
        GovernanceChange::Admit {
            member: pk(7),
            inviter: pk(2),
            expires_at: 2000,
        },
    );
    f.ingest(renew).unwrap();
    assert_eq!(f.rebuild().members[&pk(7)].inviter, pk(2));
    assert_eq!(f.rebuild().members[&pk(7)].expires_at, 2000);
    let valid_sequence = f.rebuild().governance_sequence;
    let mut cycle = f.clone();
    let rewrite = gov(
        &cycle,
        GovernanceChange::Admit {
            member: pk(2),
            inviter: pk(7),
            expires_at: 3000,
        },
    );
    let id = cycle.ingest(rewrite).unwrap();
    assert_eq!(cycle.rebuild().governance_sequence, valid_sequence);
    assert!(cycle.rebuild().pending.contains(&id));
    assert_eq!(cycle.rebuild().members[&pk(2)].inviter, pk(1));
    let self_invite = gov(
        &f,
        GovernanceChange::Admit {
            member: pk(8),
            inviter: pk(8),
            expires_at: 3000,
        },
    );
    let id = f.ingest(self_invite).unwrap();
    assert!(f.rebuild().pending.contains(&id));
    assert!(!f.rebuild().members.contains_key(&pk(8)));
}
