//! Acknowledgements mean a peer confirmed a successful save, not permanent availability.
use super::*;
impl Worker {
    pub(super) fn remote_copies(&self, id: &Digest) -> usize {
        self.replicas
            .get(id)
            .map(|peers| {
                peers
                    .iter()
                    .filter(|p| self.peers.contains(p) && self.allowed(p))
                    .count()
            })
            .unwrap_or(0)
    }
    pub(super) fn confirm_copy(&mut self, peer: PeerId, id: Digest) {
        let known = self.saved.as_ref().is_some_and(|s| {
            s.forum.records.contains_key(&id)
                || s.forum.body(&id).is_some()
                || s.cached.contains_key(&id)
        });
        if known && (self.replicas.len() < 2048 || self.replicas.contains_key(&id)) {
            self.replicas.entry(id).or_default().insert(peer);
        }
    }
    pub(super) fn offer_local(&mut self, peer: PeerId) {
        let Some(saved) = &self.saved else { return };
        let own = self.key.public().encode_protobuf();
        let offer = saved.forum.records.iter().find_map(|(id, r)| {
            if r.record.author != own || !self.shareable(id) {
                return None;
            }
            if let Action::Content { body, .. } = &r.record.action {
                if self.remote_copies(body) > 0 {
                    return None;
                }
                saved.forum.body(body).map(|bytes| Query::Offer {
                    record: r.clone(),
                    body: bytes.to_vec(),
                })
            } else {
                None
            }
        });
        if let Some(query) = offer {
            self.enqueue(peer, query);
        }
    }
    pub(super) fn accept_offer(
        &mut self,
        peer: PeerId,
        record: SignedRecord,
        body: Vec<u8>,
    ) -> Result<Response> {
        ensure!(self.allowed(&peer), "没有接收副本的成员资格");
        let Action::Content {
            body: expected,
            attachments,
            ..
        } = &record.record.action
        else {
            anyhow::bail!("副本仅接受内容记录")
        };
        ensure!(content_id(&body) == *expected, "副本正文校验失败");
        let hash = *expected;
        let attachments = attachments.iter().map(|a| a.hash).collect::<Vec<_>>();
        let saved = self.saved.as_mut().unwrap();
        let mut candidate = saved.forum.clone();
        let id = candidate.ingest(record)?;
        candidate.put_body(body)?;
        let view = candidate.rebuild();
        ensure!(
            !view.pending.contains(&id) && !view.disputed.contains(&id),
            "副本存在未验证依赖"
        );
        saved.forum = candidate;
        self.refresh();
        self.persist()?;
        for hash in attachments {
            if self.replication_downloads.len() < 10 {
                self.replication_downloads.push_back((peer, hash));
            }
        }
        Ok(Response::Accepted {
            record: id,
            body: hash,
        })
    }
    pub(super) fn replication_tick(&mut self) {
        if self.downloads.len() >= 2 {
            return;
        }
        if let Some((peer, hash)) = self.replication_downloads.pop_front() {
            if self.allowed(&peer) && self.start_download(hash).is_ok() {
                if let Some(d) = self.downloads.get(&hash) {
                    let offset = d.received;
                    self.enqueue(peer, Query::Chunk { hash, offset });
                }
            }
        }
    }
}
