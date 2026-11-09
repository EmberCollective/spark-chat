//! Deterministic derived text only: original records and their signatures remain untouched.
use super::*;

impl Forum {
    /// Returns a derived body only for two valid heads with one unambiguous merge base.
    /// None leaves the original branches visible, including missing bodies and conflicts.
    pub fn merged_body(&self, root: &Digest) -> Result<Option<String>> {
        self.merged_body_at(root, 0)
    }

    pub fn merged_body_at(&self, root: &Digest, now: u64) -> Result<Option<String>> {
        let view = self.rebuild_at(now);
        let Some(topic) = view.topics.iter().find(|topic| &topic.root == root) else {
            return Ok(None);
        };
        if topic.hidden || topic.heads.len() != 2 {
            return Ok(None);
        }
        // Head order is canonical rather than arrival order, including diffy's ours/theirs.
        let mut heads = topic.heads.clone();
        heads.sort();
        let left = self.ancestors(heads[0]);
        let right = self.ancestors(heads[1]);
        let common: BTreeSet<_> = left.intersection(&right).copied().collect();
        let mut bases = common.clone();
        for id in common {
            if let Some(SignedRecord {
                record:
                    ForumRecord {
                        action: Action::Content { parents, .. },
                        ..
                    },
                ..
            }) = self.records.get(&id)
            {
                for parent in parents {
                    bases.remove(parent);
                }
            }
        }
        if bases.len() != 1 {
            return Ok(None);
        }
        let base = *bases.first().expect("one base");
        let (Some(base), Some(left), Some(right)) = (
            self.record_text(base)?,
            self.record_text(heads[0])?,
            self.record_text(heads[1])?,
        ) else {
            return Ok(None);
        };
        // Exact dependency version is pinned: changing the merge algorithm changes views.
        let Ok(merged) = diffy::merge(base, left, right) else {
            return Ok(None);
        };
        if merged.len() > MAX_BODY_BYTES || merged.chars().count() > 20_000 {
            return Ok(None);
        }
        Ok(Some(merged))
    }

    fn ancestors(&self, head: Digest) -> BTreeSet<Digest> {
        let mut found = BTreeSet::new();
        let mut pending = vec![head];
        while let Some(id) = pending.pop() {
            if !found.insert(id) {
                continue;
            }
            if let Some(SignedRecord {
                record:
                    ForumRecord {
                        action: Action::Content { parents, .. },
                        ..
                    },
                ..
            }) = self.records.get(&id)
            {
                pending.extend(parents);
            }
        }
        found
    }

    fn record_text(&self, id: Digest) -> Result<Option<&str>> {
        let Some(SignedRecord {
            record:
                ForumRecord {
                    action: Action::Content { body, .. },
                    ..
                },
            ..
        }) = self.records.get(&id)
        else {
            return Ok(None);
        };
        self.body(body)
            .map(std::str::from_utf8)
            .transpose()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Forum, Keypair) {
        let key = Keypair::ed25519_from_bytes([1; 32]).unwrap();
        (
            Forum::new([9; 32], key.public().encode_protobuf()).unwrap(),
            key,
        )
    }

    fn content(
        f: &mut Forum,
        key: &Keypair,
        root: Option<Digest>,
        mut parents: Vec<Digest>,
        text: &str,
    ) -> Digest {
        parents.sort();
        let body = f.put_body(text.as_bytes().to_vec()).unwrap();
        let signed = sign_record(
            key,
            f.network(),
            Action::Content {
                target: root,
                reply_to: None,
                parents,
                title: "主题".into(),
                body,
                attachments: vec![],
            },
        )
        .unwrap();
        f.ingest(signed).unwrap()
    }

    #[test]
    fn independent_edits_merge_identically_after_reordered_sync_without_new_records() {
        let (mut f, key) = fixture();
        let root = content(&mut f, &key, None, vec![], "a\nb\nc\n");
        content(&mut f, &key, Some(root), vec![root], "A\nb\nc\n");
        content(&mut f, &key, Some(root), vec![root], "a\nb\nC\n");
        let count = f.records.len();
        assert_eq!(f.merged_body(&root).unwrap().as_deref(), Some("A\nb\nC\n"));
        assert_eq!(f.records.len(), count);
        let (mut reordered, _) = fixture();
        for signed in f.records.values().rev() {
            reordered.ingest(signed.clone()).unwrap();
        }
        for body in f.bodies.values().rev() {
            reordered.put_body(body.clone()).unwrap();
        }
        assert_eq!(
            f.merged_body(&root).unwrap(),
            reordered.merged_body(&root).unwrap()
        );
        assert_eq!(f.rebuild().topics, reordered.rebuild().topics);
    }

    #[test]
    fn overlap_missing_body_and_multiple_heads_preserve_branches() {
        let (mut f, key) = fixture();
        let root = content(&mut f, &key, None, vec![], "a\nb\nc\n");
        content(&mut f, &key, Some(root), vec![root], "A\nb\nc\n");
        let right = content(&mut f, &key, Some(root), vec![root], "OTHER\nb\nc\n");
        assert!(f.merged_body(&root).unwrap().is_none());
        f.records.remove(&right);
        content(&mut f, &key, Some(root), vec![root], "a\nb\nC\n");
        let mut missing = f.clone();
        missing.bodies.remove(&content_id(b"a\nb\nc\n"));
        assert!(missing.merged_body(&root).unwrap().is_none());
        content(&mut f, &key, Some(root), vec![root], "a\nB\nc\n");
        assert_eq!(f.rebuild().topics[0].heads.len(), 3);
        assert!(f.merged_body(&root).unwrap().is_none());
    }

    #[test]
    fn criss_cross_bases_remain_explicit_conflicts() {
        let (mut f, key) = fixture();
        let root = content(&mut f, &key, None, vec![], "a\nb\nc\n");
        let left = content(&mut f, &key, Some(root), vec![root], "A\nb\nc\n");
        let right = content(&mut f, &key, Some(root), vec![root], "a\nb\nC\n");
        content(&mut f, &key, Some(root), vec![left, right], "A\nb\nC\n");
        content(&mut f, &key, Some(root), vec![left, right], "A\nB\nC\n");
        assert_eq!(f.rebuild().topics[0].heads.len(), 2);
        assert!(f.merged_body(&root).unwrap().is_none());
    }
}
