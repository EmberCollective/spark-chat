//! Small-network discovery: explicit seeds, capped neighbors, two-hop content queries.
use super::*;
impl Worker {
    pub(super) fn own_address(&self) -> Option<NodeAddress> {
        if self.listen.is_empty() {
            return None;
        }
        Some(NodeAddress {
            peer: self.key.public().to_peer_id().to_string(),
            addresses: self.listen.iter().map(ToString::to_string).collect(),
        })
    }
    pub(super) fn remember(&mut self, node: NodeAddress) -> Result<()> {
        ensure!(node.addresses.len() <= 8, "节点地址过多");
        let peer: PeerId = node.peer.parse()?;
        for addr in &node.addresses {
            let addr: Multiaddr = addr.parse()?;
            ensure!(campus_address(&addr), "节点地址不在允许的校园私有范围");
        }
        if peer == self.key.public().to_peer_id() {
            return Ok(());
        }
        ensure!(self.allowed(&peer), "不能介绍未批准成员");
        if self.addresses.len() < 32 || self.addresses.contains_key(&peer) {
            self.addresses.insert(peer, node.clone());
        }
        if let Some(s) = &mut self.saved {
            if !s.entries.iter().any(|e| e.peer == node.peer) {
                if s.entries.len() >= 8 {
                    s.entries.remove(0);
                }
                s.entries.push(node);
            } else if let Some(e) = s.entries.iter_mut().find(|e| e.peer == node.peer) {
                *e = node;
            }
            self.dirty = true;
        }
        Ok(())
    }
    pub(super) fn begin_find(&mut self, hash: Digest) {
        if self.wanted.len() >= 8 {
            return;
        }
        self.wanted.insert(hash, Instant::now());
        let mut request_id = [0; 16];
        rand::thread_rng().fill_bytes(&mut request_id);
        for peer in self.peers.clone().into_iter().take(3) {
            self.enqueue(
                peer,
                Query::Find {
                    hash,
                    request_id,
                    hops: 2,
                },
            );
        }
    }
    pub(super) fn fetch_wanted(&mut self, peer: PeerId) {
        for hash in self.wanted.keys().copied().collect::<Vec<_>>() {
            if let Some(d) = self.downloads.get(&hash) {
                let offset = d.received;
                self.enqueue(peer, Query::Chunk { hash, offset });
            } else if self.attachment_size(&hash).is_none() {
                self.enqueue(peer, Query::Body { hash });
            }
        }
    }
    pub(super) fn find_answer(
        &mut self,
        peer: PeerId,
        hash: Digest,
        request_id: [u8; 16],
        hops: u8,
    ) -> Result<Response> {
        ensure!(hops <= 2, "查询跳数过大");
        let own = self.saved.as_ref().is_some_and(|s| {
            (s.forum.body(&hash).is_some()
                && s.forum.records.iter().any(|(id, r)| {
                    self.shareable(id)
                        && matches!(r.record.action,Action::Content{body,..} if body==hash)
                }))
                || (s.cached.contains_key(&hash) && self.attachment_size(&hash).is_some())
        });
        let mut nodes = self
            .providers
            .get(&hash)
            .map(|(_, nodes)| nodes.clone())
            .unwrap_or_default();
        if own {
            if let Some(node) = self.own_address() {
                nodes.insert(0, node);
            }
        }
        nodes.truncate(3);
        if !own
            && hops > 0
            && !self.seen_queries.contains_key(&request_id)
            && self.seen_queries.len() < 256
        {
            self.seen_queries.insert(request_id, Instant::now());
            for other in self
                .peers
                .clone()
                .into_iter()
                .filter(|p| *p != peer)
                .take(3)
            {
                self.enqueue(
                    other,
                    Query::Find {
                        hash,
                        request_id,
                        hops: hops - 1,
                    },
                );
            }
        }
        Ok(Response::Found { hash, nodes })
    }
    pub(super) fn discovery_tick(&mut self, swarm: &mut super::super::wire::Swarm) {
        self.providers
            .retain(|_, (at, _)| at.elapsed() < Duration::from_secs(10));
        if self.last_find.elapsed() > Duration::from_secs(2) {
            let hashes: Vec<_> = self.wanted.keys().copied().take(4).collect();
            for hash in hashes {
                let mut request_id = [0; 16];
                rand::thread_rng().fill_bytes(&mut request_id);
                for peer in self.peers.clone().into_iter().take(3) {
                    self.enqueue(
                        peer,
                        Query::Find {
                            hash,
                            request_id,
                            hops: 2,
                        },
                    );
                }
            }
            self.last_find = Instant::now();
        }
        self.seen_queries
            .retain(|_, at| at.elapsed() < Duration::from_secs(5));
        let expired: Vec<_> = self
            .wanted
            .iter()
            .filter(|(_, at)| at.elapsed() > Duration::from_secs(5))
            .map(|(h, _)| *h)
            .collect();
        for hash in expired {
            if !self.downloads.contains_key(&hash) {
                self.wanted.remove(&hash);
                self.status = "本轮未找到在线副本，可稍后重试".into();
            }
        }
        if self.peers.len() + self.authenticating.len() < 4 {
            if let Some(node) = self.candidates.pop_front() {
                if let Ok(peer) = node.peer.parse::<PeerId>() {
                    if self.allowed(&peer)
                        && !self.peers.contains(&peer)
                        && !self.authenticating.contains_key(&peer)
                    {
                        for addr in node.addresses.into_iter().take(2) {
                            if let Ok(mut addr) = addr.parse::<Multiaddr>() {
                                if campus_address(&addr) {
                                    addr.push(Protocol::P2p(peer));
                                    let _ = swarm.dial(addr);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn worker(n: u8, dir: &std::path::Path) -> Worker {
        let admin = Keypair::ed25519_from_bytes([1; 32]).unwrap();
        let mut forum = Forum::new([7; 32], admin.public().encode_protobuf()).unwrap();
        for member in 2..=3 {
            let view = forum.rebuild();
            let r = sign_record(
                &admin,
                [7; 32],
                Action::Governance {
                    sequence: view.governance_sequence + 1,
                    previous: view.governance_head,
                    change: GovernanceChange::Admit {
                        member: Keypair::ed25519_from_bytes([member; 32])
                            .unwrap()
                            .public()
                            .encode_protobuf(),
                        inviter: admin.public().encode_protobuf(),
                        expires_at: now() + 3600,
                    },
                },
            )
            .unwrap();
            forum.ingest(r).unwrap();
        }
        let mut w = Worker::new(
            Keypair::ed25519_from_bytes([n; 32]).unwrap(),
            [9; 32],
            dir.into(),
        )
        .unwrap();
        w.saved = Some(Saved {
            name: "test".into(),
            forum,
            cached: BTreeMap::new(),
            draft: DraftData::default(),
            entries: vec![],
        });
        w.refresh();
        w.listen
            .push(format!("/ip4/10.4.0.{n}/tcp/41300").parse().unwrap());
        w
    }
    #[test]
    fn two_hop_query_is_bounded_deduplicated_and_returns_provider() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let mut middle = worker(2, a.path());
        let mut provider = worker(3, b.path());
        let source = Keypair::ed25519_from_bytes([1; 32])
            .unwrap()
            .public()
            .to_peer_id();
        let middle_id = middle.key.public().to_peer_id();
        let provider_id = provider.key.public().to_peer_id();
        middle.peers.extend([source, provider_id]);
        let hash = provider
            .saved
            .as_mut()
            .unwrap()
            .forum
            .put_body(b"cached".to_vec())
            .unwrap();
        let record = sign_record(
            &provider.key,
            [7; 32],
            Action::Content {
                target: None,
                reply_to: None,
                parents: vec![],
                title: "cached".into(),
                body: hash,
                attachments: vec![],
            },
        )
        .unwrap();
        provider
            .saved
            .as_mut()
            .unwrap()
            .forum
            .ingest(record)
            .unwrap();
        provider.refresh();
        let request_id = [4; 16];
        let answer = middle.find_answer(source, hash, request_id, 2).unwrap();
        assert!(matches!(answer,Response::Found{nodes,..} if nodes.is_empty()));
        assert_eq!(middle.queues[&provider_id].len(), 1);
        middle.find_answer(source, hash, request_id, 2).unwrap();
        assert_eq!(middle.queues[&provider_id].len(), 1);
        assert!(middle.find_answer(source, hash, [5; 16], 3).is_err());
        let query = middle
            .queues
            .get_mut(&provider_id)
            .unwrap()
            .pop_front()
            .unwrap();
        let response = provider
            .find_answer(middle_id, hash, request_id, 1)
            .unwrap();
        middle
            .receive(
                Pending {
                    peer: provider_id,
                    query,
                },
                response,
            )
            .unwrap();
        let result = middle.find_answer(source, hash, [6; 16], 2).unwrap();
        assert!(
            matches!(result,Response::Found{nodes,..} if nodes.len()==1&&nodes[0].peer==provider_id.to_string())
        );
    }
}
