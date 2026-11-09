//! Local encrypted community catalog; each community keeps its own service and storage directory.
use super::service::{ForumRequest, ForumService};
use anyhow::{ensure, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{cell::RefCell, fs::File, path::PathBuf};
use zeroize::Zeroize;
#[derive(Clone, Default, Serialize, Deserialize)]
struct Catalog {
    active: String,
    communities: Vec<Community>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Community {
    id: String,
    name: String,
    admin: bool,
}
struct Active {
    catalog: Catalog,
    service: ForumService,
}
pub struct ForumHub {
    seed: [u8; 32],
    key: [u8; 32],
    root: PathBuf,
    active: RefCell<Active>,
}
impl Drop for ForumHub {
    fn drop(&mut self) {
        self.seed.zeroize();
        self.key.zeroize();
    }
}
impl ForumHub {
    pub fn start(seed: [u8; 32], key: [u8; 32], root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        let path = root.join("communities.bin");
        let mut catalog: Catalog = if path.exists() {
            postcard::from_bytes(&zeroize::Zeroizing::new(super::storage::load_bytes(
                &path, &key,
            )?))?
        } else {
            Catalog {
                active: "legacy".into(),
                communities: vec![],
            }
        };
        if catalog.active.is_empty() {
            catalog.active = "legacy".into();
        }
        let directory = Self::directory(&root, &catalog.active)?;
        let service = ForumService::start(seed, key, directory)?;
        let hub = Self {
            seed,
            key,
            root,
            active: RefCell::new(Active { catalog, service }),
        };
        hub.handle(ForumRequest::State)?;
        Ok(hub)
    }
    fn directory(root: &std::path::Path, id: &str) -> Result<PathBuf> {
        ensure!(
            id == "legacy" || (id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit())),
            "社区标识无效"
        );
        Ok(if id == "legacy" {
            root.to_owned()
        } else {
            root.join("communities").join(id)
        })
    }
    fn save(&self, catalog: &Catalog) -> Result<()> {
        super::storage::save_bytes(
            &self.root.join("communities.bin"),
            &self.key,
            &zeroize::Zeroizing::new(postcard::to_allocvec(catalog)?),
        )
    }
    pub fn current_id(&self) -> String {
        self.active.borrow().catalog.active.clone()
    }
    pub fn check_scope(&self, expected: Option<&str>, unscoped: bool) -> Result<()> {
        match expected {
            Some(id) => ensure!(id == self.current_id(), "社区已切换，请重新操作"),
            None => ensure!(
                unscoped || self.active.borrow().catalog.communities.is_empty(),
                "请指定当前社区"
            ),
        };
        Ok(())
    }
    pub fn handle(&self, request: ForumRequest) -> Result<Value> {
        let mut active = self.active.borrow_mut();
        if let ForumRequest::CommunitySelect { id } = request {
            ensure!(
                active.catalog.communities.iter().any(|c| c.id == id),
                "尚未加入该社区"
            );
            if id != active.catalog.active {
                let service =
                    ForumService::start(self.seed, self.key, Self::directory(&self.root, &id)?)?;
                let mut catalog = active.catalog.clone();
                catalog.active = id;
                self.save(&catalog)?;
                *active = Active { catalog, service };
            }
            return self.decorate(&mut active, None);
        }
        if matches!(request, ForumRequest::CommunityList) {
            return self.decorate(&mut active, None);
        }
        let creating = matches!(
            &request,
            ForumRequest::Create { .. } | ForumRequest::Join { .. }
        );
        let joined = active.service.handle(ForumRequest::State)?["state"]["joined"] == true;
        let result = if creating && joined {
            let mut bytes = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut bytes);
            let id = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
            let service =
                ForumService::start(self.seed, self.key, Self::directory(&self.root, &id)?)?;
            let result = service.handle(request)?;
            let mut catalog = active.catalog.clone();
            catalog.active = id;
            let state = &result["state"];
            catalog.communities.push(Community {
                id: catalog.active.clone(),
                name: state["name"].as_str().unwrap_or("").into(),
                admin: state["admin"] == true,
            });
            self.save(&catalog)?;
            *active = Active { catalog, service };
            result
        } else {
            active.service.handle(request)?
        };
        self.decorate(&mut active, Some(result))
    }
    fn decorate(&self, active: &mut Active, result: Option<Value>) -> Result<Value> {
        let mut result = match result {
            Some(v) => v,
            None => active.service.handle(ForumRequest::State)?,
        };
        if result["state"]["joined"] == true
            && !active
                .catalog
                .communities
                .iter()
                .any(|c| c.id == active.catalog.active)
        {
            active.catalog.communities.push(Community {
                id: active.catalog.active.clone(),
                name: result["state"]["name"].as_str().unwrap_or("").into(),
                admin: result["state"]["admin"] == true,
            });
            self.save(&active.catalog)?;
        }
        result["state"]["communities"] = json!(active.catalog.communities);
        result["state"]["current_community"] = json!(active.catalog.active);
        Ok(result)
    }
    pub fn attach(&self, name: String, file: File) -> Result<Value> {
        self.active.borrow().service.attach(name, file)
    }
    pub fn export(&self, hash: String, file: File) -> Result<Value> {
        self.active.borrow().service.export(hash, file)
    }
    pub fn read_export(&self, hash: String) -> Result<Vec<u8>> {
        self.active.borrow().service.read_export(hash)
    }
    pub fn read_preview(&self, hash: String) -> Result<Value> {
        self.active.borrow().service.read_preview(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_and_new_communities_keep_independent_state_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = ForumService::start([1; 32], [2; 32], dir.path().into()).unwrap();
        legacy
            .handle(ForumRequest::Create {
                name: "原社区".into(),
            })
            .unwrap();
        legacy
            .handle(ForumRequest::Nickname {
                nickname: "原昵称".into(),
            })
            .unwrap();
        drop(legacy);
        let hub = ForumHub::start([1; 32], [2; 32], dir.path().into()).unwrap();
        let original = hub.handle(ForumRequest::State).unwrap();
        assert_eq!(original["state"]["name"], "原社区");
        assert_eq!(original["state"]["nickname"], "原昵称");
        let next = hub
            .handle(ForumRequest::Create {
                name: "新社区".into(),
            })
            .unwrap();
        assert_eq!(next["state"]["communities"].as_array().unwrap().len(), 2);
        assert_eq!(next["state"]["admin"], true);
        assert!(next["state"]["nickname"].is_null());
        let new_id = hub.current_id();
        assert!(hub.check_scope(Some("legacy"), false).is_err());
        assert!(hub.check_scope(None, false).is_err());
        assert!(hub.check_scope(Some(&new_id), false).is_ok());
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = ForumService::start([3; 32], [4; 32], remote_dir.path().into()).unwrap();
        remote
            .handle(ForumRequest::Create {
                name: "他人社区".into(),
            })
            .unwrap();
        let invitation = remote
            .handle(ForumRequest::Admit {
                member: next["state"]["own_public"].as_str().unwrap().into(),
                inviter: None,
                expires_at: None,
            })
            .unwrap()["code"]
            .as_str()
            .unwrap()
            .to_owned();
        let joined = hub.handle(ForumRequest::Join { invitation }).unwrap();
        assert_eq!(joined["state"]["admin"], false);
        assert_eq!(joined["state"]["communities"].as_array().unwrap().len(), 3);
        hub.handle(ForumRequest::CommunitySelect { id: new_id.clone() })
            .unwrap();
        hub.handle(ForumRequest::Nickname {
            nickname: "新昵称".into(),
        })
        .unwrap();
        hub.handle(ForumRequest::CommunitySelect {
            id: "legacy".into(),
        })
        .unwrap();
        assert_eq!(
            hub.handle(ForumRequest::State).unwrap()["state"]["nickname"],
            "原昵称"
        );
        drop(hub);
        let hub = ForumHub::start([1; 32], [2; 32], dir.path().into()).unwrap();
        hub.handle(ForumRequest::CommunitySelect { id: new_id })
            .unwrap();
        assert_eq!(
            hub.handle(ForumRequest::State).unwrap()["state"]["nickname"],
            "新昵称"
        );
        assert!(hub
            .handle(ForumRequest::CommunitySelect {
                id: "../outside".into()
            })
            .is_err());
        drop(hub);
        assert!(ForumHub::start([1; 32], [2; 32], dir.path().into()).is_ok());
    }
}
