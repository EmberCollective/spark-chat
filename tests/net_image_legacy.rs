//! A peer advertising only the original chat protocol rejects the image extension without losing text chat.
use futures::StreamExt;
use libp2p::{request_response::{Behaviour, Config, Event, Message, ProtocolSupport}, swarm::SwarmEvent, StreamProtocol, SwarmBuilder};
use spark_chat::{net::{self, FrameCodec, NetCmd, NetEvent}, protocol::{self, ConnectInfo, Frame}};
use std::{sync::mpsc, time::Duration};

#[test]
fn legacy_protocol_rejects_image_and_keeps_text_connection() {
    let (address_tx, address_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let legacy = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
            let key = libp2p::identity::Keypair::ed25519_from_bytes([9; 32]).unwrap();
            let mut swarm = SwarmBuilder::with_existing_identity(key).with_tokio()
                .with_tcp(libp2p::tcp::Config::default(), libp2p::noise::Config::new, libp2p::yamux::Config::default).unwrap()
                .with_behaviour(|_| Behaviour::with_codec(FrameCodec, [(StreamProtocol::new("/spark-chat/1.0.0"), ProtocolSupport::Full)], Config::default())).unwrap()
                .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60))).build();
            swarm.listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap()).unwrap();
            let mut timer = tokio::time::interval(Duration::from_millis(20));
            loop {
                tokio::select! {
                    _ = timer.tick() => if stop_rx.try_recv().is_ok() { break; },
                    event = swarm.select_next_some() => match event {
                        SwarmEvent::NewListenAddr { address, .. } => {
                            let info = ConnectInfo { addrs: vec![address.to_string()], peer_id: swarm.local_peer_id().to_string(), salt: [0; 4] };
                            address_tx.send(protocol::encode_conn_code(&info).unwrap()).unwrap();
                        },
                        SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                            swarm.behaviour_mut().send_request(&peer_id, Frame::Text { seq: 0, ts: 0, text: format!("@HELLO {}", "37".repeat(32)) });
                        },
                        SwarmEvent::Behaviour(Event::Message { peer, message: Message::Request { request, channel, .. }, .. }) => {
                            swarm.behaviour_mut().send_response(channel, ()).unwrap();
                            if matches!(request, Frame::Text { ref text, .. } if text == "still text") {
                                swarm.behaviour_mut().send_request(&peer, Frame::Text { seq: 1, ts: 1, text: "text works".into() });
                            }
                        },
                        _ => {},
                    }
                }
            }
        });
    });
    let code = address_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let (tx, rx) = net::spawn([10; 32], [20; 32]);
    tx.send(NetCmd::Dial(code)).unwrap();
    loop {
        if matches!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), NetEvent::PeerKey { .. }) { break; }
    }
    tx.send(NetCmd::Ready).unwrap();
    tx.send(NetCmd::SendImage(vec![1, 2, 3])).unwrap();
    let result = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(result, NetEvent::Error(ref reason) if reason.contains("图片发送失败") && reason.contains("旧客户端")), "{result:?}");
    tx.send(NetCmd::SendText("still text".into())).unwrap();
    assert!(matches!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), NetEvent::Msg(text) if text == "text works"));
    tx.send(NetCmd::Shutdown).unwrap();
    drop(tx);
    stop_tx.send(()).unwrap();
    legacy.join().unwrap();
}
