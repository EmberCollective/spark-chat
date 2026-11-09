//! Real local TCP/Noise nodes: no application mocks and no campus availability claims.
use serde_json::Value;
use spark_chat::forum::service::{ForumRequest, ForumService};
use std::time::{Duration, Instant};

fn state(node: &ForumService) -> Value {
    node.handle(ForumRequest::Page { offset: 0 }).unwrap()["state"].clone()
}

fn wait(label: &str, mut predicate: impl FnMut() -> bool) {
    let start = Instant::now();
    while !predicate() {
        assert!(
            start.elapsed() < Duration::from_secs(25),
            "timeout: {label}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn entry(node: &ForumService) -> String {
    node.handle(ForumRequest::Listen).unwrap();
    let mut code = None;
    wait("listen", || {
        code = node.handle(ForumRequest::State).unwrap()["code"]
            .as_str()
            .map(str::to_owned);
        code.is_some()
    });
    code.unwrap()
}

fn connect(from: &ForumService, to: &ForumService) {
    from.handle(ForumRequest::Connect { code: entry(to) })
        .unwrap();
}

fn publish(
    node: &ForumService,
    target: Option<String>,
    reply_to: Option<String>,
    parents: Vec<String>,
    title: &str,
    body: &str,
) {
    node.handle(ForumRequest::Publish {
        target,
        reply_to,
        parents,
        title: title.into(),
        body: body.into(),
        attachments: vec![],
    })
    .unwrap();
}

fn topic(value: &Value, root: &str) -> Value {
    value["topics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == root)
        .cloned()
        .unwrap()
}

fn projection(mut value: Value) -> Value {
    for topic in value["topics"].as_array_mut().unwrap() {
        for version in topic["versions"].as_array_mut().unwrap() {
            for key in ["body", "body_copies", "record_copies"] {
                version.as_object_mut().unwrap().remove(key);
            }
            for attachment in version["attachments"].as_array_mut().unwrap() {
                attachment.as_object_mut().unwrap().remove("copies");
            }
        }
        topic.as_object_mut().unwrap().remove("merged_body");
    }
    value["topics"].clone()
}

#[test]
fn six_members_reconcile_three_partitions_without_admin_and_keep_offline_author_content() {
    let admin_dir = tempfile::tempdir().unwrap();
    let admin = ForumService::start([1; 32], [51; 32], admin_dir.path().into()).unwrap();
    admin
        .handle(ForumRequest::Create {
            name: "六节点分区验收".into(),
        })
        .unwrap();
    let dirs: Vec<_> = (0..6).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut nodes: Vec<Option<ForumService>> = dirs
        .iter()
        .enumerate()
        .map(|(i, dir)| {
            Some(
                ForumService::start([i as u8 + 2; 32], [i as u8 + 20; 32], dir.path().into())
                    .unwrap(),
            )
        })
        .collect();
    publish(&admin, None, None, vec![], "共同原帖", "原文\n第二行\n");
    let root = state(&admin)["topics"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    admin
        .handle(ForumRequest::Checkpoint {
            records: vec![root.clone()],
        })
        .unwrap();
    for node in nodes.iter().flatten() {
        admin
            .handle(ForumRequest::Admit {
                member: state(node)["own_public"].as_str().unwrap().into(),
                inviter: None,
                expires_at: None,
            })
            .unwrap();
    }
    // Cold invitations carry signed checkpoint evidence, but no live peer addresses.
    // Never connect all members to one host: neighbor discovery would heal the intended partition.
    for node in nodes.iter().flatten() {
        let invitation = admin
            .handle(ForumRequest::Admit {
                member: state(node)["own_public"].as_str().unwrap().into(),
                inviter: None,
                expires_at: None,
            })
            .unwrap()["code"]
            .as_str()
            .unwrap()
            .to_owned();
        node.handle(ForumRequest::Join { invitation }).unwrap();
        assert_eq!(state(node)["topics"].as_array().unwrap().len(), 1);
        assert_eq!(state(node)["peers"], 0);
    }
    drop(admin);

    for (a, b) in [(0, 1), (2, 3), (4, 5)] {
        connect(nodes[a].as_ref().unwrap(), nodes[b].as_ref().unwrap());
    }
    wait("three disconnected pairs", || {
        nodes.iter().flatten().all(|n| state(n)["peers"] == 1)
    });
    for (group, leader) in [0, 2, 4].into_iter().enumerate() {
        let node = nodes[leader].as_ref().unwrap();
        publish(
            node,
            Some(root.clone()),
            None,
            vec![root.clone()],
            "共同原帖",
            &format!("分区{group}修改\n第二行\n"),
        );
        publish(
            node,
            None,
            None,
            vec![],
            &format!("独立主题{group}"),
            "本分区新增",
        );
        publish(
            node,
            None,
            Some(root.clone()),
            vec![],
            "",
            &format!("分区{group}回复"),
        );
    }
    wait("local partition publication", || {
        nodes.iter().flatten().all(|n| {
            state(n)["topics"].as_array().unwrap().len() == 2
                && n.handle(ForumRequest::Topic {
                    id: root.clone(),
                    offset: 0,
                })
                .unwrap()["state"]["reply_total"]
                    == 1
        })
    });
    connect(nodes[0].as_ref().unwrap(), nodes[2].as_ref().unwrap());
    connect(nodes[0].as_ref().unwrap(), nodes[4].as_ref().unwrap());
    for node in nodes.iter().flatten() {
        node.handle(ForumRequest::Sync).unwrap();
    }
    wait("all partition indices converge", || {
        let values: Vec<_> = nodes.iter().flatten().map(state).collect();
        values.iter().all(|v| {
            v["topics"].as_array().unwrap().len() == 4
                && topic(v, &root)["heads"].as_array().unwrap().len() == 3
        }) && values
            .iter()
            .all(|v| projection(v.clone()) == projection(values[0].clone()))
            && {
                let threads: Vec<_> = nodes
                    .iter()
                    .flatten()
                    .map(|n| {
                        n.handle(ForumRequest::Topic {
                            id: root.clone(),
                            offset: 0,
                        })
                        .unwrap()["state"]
                            .clone()
                    })
                    .collect();
                threads.iter().all(|v| {
                    v["reply_total"] == 3 && projection(v.clone()) == projection(threads[0].clone())
                })
            }
    });
    let before = state(nodes[0].as_ref().unwrap());
    let heads = topic(&before, &root)["heads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    publish(
        nodes[0].as_ref().unwrap(),
        Some(root.clone()),
        None,
        heads,
        "共同原帖",
        "已明确解决三个分支\n",
    );
    wait("signed conflict resolution", || {
        nodes.iter().flatten().all(|n| {
            let t = topic(&state(n), &root);
            t["heads"].as_array().unwrap().len() == 1
                && t["versions"].as_array().unwrap().len() == 5
        })
    });
    let resolved = topic(&state(nodes[0].as_ref().unwrap()), &root);
    let head = &resolved["heads"][0];
    let hash = resolved["versions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| &v["id"] == head)
        .unwrap()["body_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let has_body = |node: &ForumService| {
        topic(&state(node), &root)["versions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["body"] == "已明确解决三个分支\n")
    };
    for index in [2, 4] {
        nodes[index]
            .as_ref()
            .unwrap()
            .handle(ForumRequest::Fetch { hash: hash.clone() })
            .unwrap();
        wait("replica fetch", || has_body(nodes[index].as_ref().unwrap()));
    }
    drop(nodes[0].take());
    connect(nodes[1].as_ref().unwrap(), nodes[2].as_ref().unwrap());
    wait("replacement connection", || {
        state(nodes[1].as_ref().unwrap())["peers"].as_u64().unwrap() > 0
    });
    for index in [1, 3, 5] {
        nodes[index]
            .as_ref()
            .unwrap()
            .handle(ForumRequest::Fetch { hash: hash.clone() })
            .unwrap();
        wait("fetch with original author offline", || {
            has_body(nodes[index].as_ref().unwrap())
        });
    }
    drop(nodes[3].take());
    let restarted = ForumService::start([5; 32], [23; 32], dirs[3].path().into()).unwrap();
    assert!(has_body(&restarted), "完整缓存应在重启后仍可读取");
}
