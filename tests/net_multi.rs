//! A single listening node accepts two peers, isolates matching file IDs and survives one disconnect.
use spark_chat::{
    net::{self, NetCmd, NetEvent},
    protocol,
    transfer::ReceiveTarget,
};
use std::{
    sync::mpsc::{Receiver, Sender},
    time::{Duration, Instant},
};

fn wait(rx: &Receiver<NetEvent>, predicate: impl Fn(&NetEvent) -> bool) -> NetEvent {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let event = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("network event timed out");
        assert!(!matches!(&event, NetEvent::Error(_)), "{event:?}");
        if predicate(&event) {
            return event;
        }
    }
}
fn targeted(tx: &Sender<NetCmd>, peer: &str, command: NetCmd) {
    tx.send(NetCmd::To {
        peer_id: peer.into(),
        command: Box::new(command),
    })
    .unwrap();
}
#[test]
fn two_inbound_peers_remain_independent() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    std::env::set_var("SPARK_PORT", port.to_string());
    let (a, ar) = net::spawn_multi([1; 32], [11; 32]);
    let (b, br) = net::spawn([2; 32], [22; 32]);
    let (c, cr) = net::spawn([3; 32], [33; 32]);
    a.send(NetCmd::StartListen).unwrap();
    let NetEvent::ListenCode { code, .. } = wait(&ar, |e| matches!(e, NetEvent::ListenCode { .. }))
    else {
        unreachable!()
    };
    let mut info = protocol::decode_conn_code(&code).unwrap();
    info.addrs = vec![format!("/ip4/127.0.0.1/tcp/{port}")];
    let code = protocol::encode_conn_code(&info).unwrap();
    b.send(NetCmd::Dial(code.clone())).unwrap();
    wait(&br, |e| matches!(e, NetEvent::PeerKey { .. }));
    let NetEvent::PeerEvent { peer_id: b_id, .. } = wait(
        &ar,
        |e| matches!(e, NetEvent::PeerEvent { event, .. } if matches!(event.as_ref(), NetEvent::PeerKey { .. })),
    ) else {
        unreachable!()
    };
    c.send(NetCmd::Dial(code)).unwrap();
    wait(&cr, |e| matches!(e, NetEvent::PeerKey { .. }));
    let NetEvent::PeerEvent { peer_id: c_id, .. } = wait(
        &ar,
        |e| matches!(e, NetEvent::PeerEvent { event, .. } if matches!(event.as_ref(), NetEvent::PeerKey { .. })),
    ) else {
        unreachable!()
    };
    assert_ne!(b_id, c_id);
    a.send(NetCmd::StartListen).unwrap();
    wait(&ar, |e| matches!(e, NetEvent::ListenCode { .. }));
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 2).write_to(&mut encoded, image::ImageFormat::Png).unwrap();
    let image = encoded.into_inner();
    b.send(NetCmd::Ready).unwrap();
    b.send(NetCmd::SendImage(image.clone())).unwrap();
    let denied = br.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(denied, NetEvent::Error(ref reason) if reason.contains("未被接收")), "image rejected until App approves peer: {denied:?}");
    targeted(&a, &b_id, NetCmd::Ready);
    b.send(NetCmd::SendImage(image.clone())).unwrap();
    let NetEvent::PeerEvent { peer_id, event } = wait(&ar, |e| matches!(e, NetEvent::PeerEvent { event, .. } if matches!(event.as_ref(), NetEvent::ImageIncoming { .. }))) else { unreachable!() };
    assert_eq!(peer_id, b_id);
    let NetEvent::ImageIncoming { id, bytes } = *event else { unreachable!() };
    assert_eq!(bytes, image);
    targeted(&a, &b_id, NetCmd::ImageResult { id, result: Ok(()) });
    wait(&br, |e| matches!(e, NetEvent::ImageDelivered));
    // An oversized outgoing image is refused before it reaches the other peer.
    b.send(NetCmd::SendImage(vec![0; spark_chat::images::MAX_IMAGE_BYTES + 1])).unwrap();
    assert!(matches!(br.recv_timeout(Duration::from_secs(5)).unwrap(), NetEvent::Error(_)));
    // Reusing the same image content is a new deliberate send, not a duplicate ID collision.
    b.send(NetCmd::SendImage(image.clone())).unwrap();
    c.send(NetCmd::SendText("parallel C message".into())).unwrap();
    let mut saw_c = false;
    let mut saw_image = false;
    while !saw_c || !saw_image {
        let ev = ar.recv_timeout(Duration::from_secs(5)).unwrap();
        match ev {
            NetEvent::PeerEvent { peer_id, event } => match *event {
                NetEvent::ImageIncoming { id, bytes } => {
                    assert_eq!(peer_id, b_id);
                    assert_eq!(bytes, image);
                    targeted(&a, &b_id, NetCmd::ImageResult { id, result: Ok(()) });
                    saw_image = true;
                },
                NetEvent::Msg(text) => { assert_eq!(peer_id, c_id); assert_eq!(text, "parallel C message"); saw_c = true; },
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("unexpected {other:?}"),
        }
    }
    wait(&br, |e| matches!(e, NetEvent::ImageDelivered));
    targeted(&a, &b_id, NetCmd::SendText("only B".into()));
    targeted(&a, &c_id, NetCmd::SendText("only C".into()));
    assert!(
        matches!(wait(&br, |e| matches!(e, NetEvent::Msg(_))), NetEvent::Msg(s) if s == "only B")
    );
    assert!(
        matches!(wait(&cr, |e| matches!(e, NetEvent::Msg(_))), NetEvent::Msg(s) if s == "only C")
    );

    let tmp = tempfile::tempdir().unwrap();
    for (tx, name, data) in [
        (&b, "b", vec![42; protocol::CHUNK_SIZE + 11]),
        (&c, "c", vec![73; protocol::CHUNK_SIZE + 7]),
    ] {
        let path = tmp.path().join(name);
        std::fs::write(&path, data).unwrap();
        tx.send(NetCmd::SendFile(path)).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut ready = std::collections::HashSet::new();
    while ready.len() < 2 {
        let ev = ar
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        let NetEvent::PeerEvent { peer_id, event } = ev else {
            panic!("unexpected {ev:?}")
        };
        match *event {
            NetEvent::FileIncoming { id, size, .. } => {
                assert_eq!(
                    id, 1,
                    "separate peers deliberately reuse their first transfer ID"
                );
                let path = tmp.path().join(format!("received-{peer_id}"));
                targeted(
                    &a,
                    &peer_id,
                    NetCmd::AcceptFile {
                        id,
                        target: ReceiveTarget::path(&path, size).unwrap(),
                    },
                );
            }
            NetEvent::FileReady { id, .. } => {
                assert_eq!(id, 1);
                let expected = if peer_id == b_id {
                    vec![42; protocol::CHUNK_SIZE + 11]
                } else {
                    vec![73; protocol::CHUNK_SIZE + 7]
                };
                assert_eq!(
                    std::fs::read(tmp.path().join(format!("received-{peer_id}"))).unwrap(),
                    expected
                );
                ready.insert(peer_id);
            }
            NetEvent::Error(e) => panic!("{e}"),
            _ => {}
        }
    }
    b.send(NetCmd::Shutdown).unwrap();
    drop(b);
    wait(
        &ar,
        |e| matches!(e, NetEvent::PeerEvent { peer_id, event } if peer_id == &b_id && matches!(event.as_ref(), NetEvent::Disconnected(_))),
    );
    targeted(&a, &c_id, NetCmd::SendText("C survives B leaving".into()));
    assert!(
        matches!(wait(&cr, |e| matches!(e, NetEvent::Msg(_))), NetEvent::Msg(s) if s == "C survives B leaving")
    );
    a.send(NetCmd::Shutdown).unwrap();
    c.send(NetCmd::Shutdown).unwrap();
}
