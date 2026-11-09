//! 集成层：密钥留在 Rust，界面只收到可显示的状态与消息。
use crate::net::{self, NetCmd, NetEvent};
use crate::netpos::{self, Position};
use crate::ui::{AppHooks, NetPosKind, UiEvent, UiMsg, UiImage};
use crate::vault::{Session, Vault};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

const MAX_TEXT: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct InitialFile {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub outgoing: bool,
    pub complete: bool,
    pub accepted: bool,
    pub failed: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InitialSession {
    pub images: Vec<UiImage>,
    pub id: String,
    pub connected: bool,
    pub history: Vec<UiMsg>,
    pub files: Vec<InitialFile>,
}

#[derive(Default)]
struct ChatState {
    session: Option<Session>,
    peer_pid: Option<String>,
    peer_key: Option<[u8; 32]>,
    msgs: Vec<UiMsg>,
    early_msgs: Vec<UiMsg>,
    early_overflowed: bool,
    files: HashMap<u64, String>,
    transfers: HashMap<(bool, u64), InitialFile>,
    incoming_ids: HashMap<u64, u64>,
}

#[derive(Serialize)]
pub struct InitialState {
    pub images: Vec<UiImage>,
    pub active_session: String,
    pub sessions: Vec<InitialSession>,
    pub vault_created: bool,
    pub mobile: bool,
    pub keyfile: String,
    pub my_id: String,
    pub position: String,
    pub position_kind: String,
    pub history: Vec<UiMsg>,
    pub connected: bool,
    pub peer_id: String,
    pub files: Vec<InitialFile>,
    pub revision: u64,
}

#[derive(Default, Deserialize, Serialize)]
struct Prefs {
    vault_created: bool,
    keyfile: Option<String>,
}

pub struct App {
    image_cache: Option<crate::images::ImageCache>,
    images: HashMap<String, Vec<UiImage>>,
    chats: HashMap<String, ChatState>,
    multi: bool,
    dir: PathBuf,
    vault: Option<Vault>,
    session: Option<Session>,
    peer_pid: Option<String>,
    peer_key: Option<[u8; 32]>,
    net_cmd: Option<Sender<NetCmd>>,
    net_evt: Option<Receiver<NetEvent>>,
    msgs: Vec<UiMsg>,
    early_msgs: Vec<UiMsg>,
    early_overflowed: bool,
    files: HashMap<u64, String>,
    transfers: HashMap<(bool, u64), InitialFile>,
    revision: u64,
    incoming_ids: HashMap<u64, u64>,
    next_received_id: u64,
    my_pid: String,
    pos_sent: bool,
    errors: Vec<UiEvent>,
}

impl App {
    pub fn new() -> Self {
        // Debug-only QA isolation keeps disposable vaults away from real user data.
        #[cfg(debug_assertions)]
        if let Some(dir) = std::env::var_os("SPARK_TEST_DATA_DIR") {
            return Self::with_dir(PathBuf::from(dir));
        }
        Self::with_dir(
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("spark-chat"),
        )
    }

    pub(crate) fn with_dir(dir: PathBuf) -> Self {
        Self {
            image_cache: None,
            images: HashMap::new(),
            chats: HashMap::new(),
            multi: false,
            dir,
            vault: None,
            session: None,
            peer_pid: None,
            peer_key: None,
            net_cmd: None,
            net_evt: None,
            msgs: Vec::new(),
            early_msgs: Vec::new(),
            early_overflowed: false,
            files: HashMap::new(),
            transfers: HashMap::new(),
            revision: 0,
            incoming_ids: HashMap::new(),
            next_received_id: 1,
            my_pid: String::new(),
            pos_sent: false,
            errors: Vec::new(),
        }
    }

    fn open_image_cache(&mut self, vault: &Vault) -> Result<(), String> {
        use zeroize::Zeroize;
        let mut seed = vault.ed25519_seed();
        let mut key = [0u8; 32];
        let derived = hkdf::Hkdf::<sha2::Sha256>::new(None, &seed).expand(b"spark-chat/private-image-cache/v1", &mut key);
        seed.zeroize();
        derived.map_err(|_| "图片缓存密钥派生失败")?;
        let cache = crate::images::ImageCache::new(self.dir.join("private-images"), key).map_err(|e| e.to_string());
        key.zeroize();
        let cache = cache?;
        let index = cache.load_index().map_err(|e| format!("图片索引无法读取：{e}"))?;
        let images: HashMap<String, Vec<UiImage>> = match index {
            Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("图片索引损坏，原文件已保留：{e}"))?,
            None => HashMap::new(),
        };
        if images.values().map(Vec::len).sum::<usize>() > 128 { return Err("图片索引数量异常".into()); }
        for peer in images.keys() {
            self.chats.entry(peer.clone()).or_insert_with(|| ChatState { peer_pid: Some(peer.clone()), ..Default::default() });
        }
        self.images = images;
        self.image_cache = Some(cache);
        Ok(())
    }

    fn store_image(&mut self, peer: &str, bytes: &[u8], own: bool) -> Result<UiImage, String> {
        let cache = self.image_cache.as_ref().ok_or("图片缓存尚未解锁")?;
        let id = cache.put(bytes).map_err(|e| e.to_string())?;
        let normalized = cache.get(&id).map_err(|e| e.to_string())?;
        let image = UiImage { id, own, ts: now_ms(), mime: normalized.mime };
        self.images.entry(peer.into()).or_default().push(image.clone());
        let result = serde_json::to_vec(&self.images).map_err(|e| e.to_string()).and_then(|bytes| cache.save_index(&bytes).map_err(|e| e.to_string()));
        if let Err(error) = result {
            self.images.get_mut(peer).unwrap().pop();
            return Err(format!("图片索引未保存：{error}"));
        }
        Ok(image)
    }

    pub fn image_to(&mut self, peer: &str, bytes: &[u8]) -> Result<UiImage, String> {
        self.for_session(peer, |app| {
            if app.session.is_none() { return Err("请先连接对方再发送图片".into()); }
            let image = app.store_image(peer, bytes, true)?;
            let data = app.image_cache.as_ref().unwrap().get(&image.id).map_err(|e| e.to_string())?;
            app.send(NetCmd::SendImage(data.bytes)).map_err(|e| format!("图片已保存在本机但未发送：{e}"))?;
            Ok(image)
        })
    }

    pub fn image_preview(&self, peer: &str, id: &str) -> Result<String, String> {
        use base64::Engine;
        if !self.images.get(peer).is_some_and(|images| images.iter().any(|image| image.id == id)) {
            return Err("图片不属于该会话或已失效".into());
        }
        let image = self.image_cache.as_ref().ok_or("请先解锁")?.get(id).map_err(|e| e.to_string())?;
        Ok(base64::engine::general_purpose::STANDARD.encode(image.bytes))
    }

    pub(crate) fn forum_material(&self) -> Result<([u8; 32], PathBuf), String> {
        let vault = self.vault.as_ref().ok_or("请先解锁")?;
        Ok((vault.ed25519_seed(), self.dir.clone()))
    }

    pub fn active_session(&self) -> String {
        self.peer_pid.clone().unwrap_or_default()
    }

    fn swap_chat(&mut self, state: &mut ChatState) {
        std::mem::swap(&mut self.session, &mut state.session);
        std::mem::swap(&mut self.peer_pid, &mut state.peer_pid);
        std::mem::swap(&mut self.peer_key, &mut state.peer_key);
        std::mem::swap(&mut self.msgs, &mut state.msgs);
        std::mem::swap(&mut self.early_msgs, &mut state.early_msgs);
        std::mem::swap(&mut self.early_overflowed, &mut state.early_overflowed);
        std::mem::swap(&mut self.files, &mut state.files);
        std::mem::swap(&mut self.transfers, &mut state.transfers);
        std::mem::swap(&mut self.incoming_ids, &mut state.incoming_ids);
    }

    fn activate(&mut self, id: &str) {
        if id.is_empty() {
            if let Some(previous) = self.peer_pid.clone() {
                let mut state = ChatState::default();
                self.swap_chat(&mut state);
                self.chats.insert(previous, state);
            }
            return;
        }
        if self.peer_pid.as_deref() == Some(id) {
            return;
        }
        let previous = self.peer_pid.clone();
        let mut state = self.chats.remove(id).unwrap_or_else(|| ChatState {
            peer_pid: Some(id.into()),
            ..Default::default()
        });
        self.swap_chat(&mut state);
        if let Some(previous) = previous {
            self.chats.insert(previous, state);
        }
    }

    pub fn select_session(&mut self, id: &str) -> Result<InitialState, String> {
        if self.peer_pid.as_deref() != Some(id) && !self.chats.contains_key(id) {
            return Err("会话不存在".into());
        }
        self.activate(id);
        Ok(self.initial_state())
    }

    fn for_session<T>(
        &mut self,
        id: &str,
        operation: impl FnOnce(&mut Self) -> Result<T, String>,
    ) -> Result<T, String> {
        if self.peer_pid.as_deref() != Some(id) && !self.chats.contains_key(id) {
            return Err("会话不存在".into());
        }
        let previous = self.active_session();
        self.activate(id);
        let first = self.errors.len();
        let result = operation(self);
        if self.multi {
            for event in &mut self.errors[first..] {
                let original = std::mem::replace(event, UiEvent::Error(String::new()));
                *event = UiEvent::SessionEvent {
                    session_id: id.into(),
                    event: Box::new(original),
                };
            }
        }
        self.activate(&previous);
        result
    }

    pub fn text_to(&mut self, id: &str, text: &str) -> Result<UiMsg, String> {
        self.for_session(id, |app| app.text(text))
    }
    pub fn send_path_to(&mut self, id: &str, path: &Path) -> Result<(), String> {
        self.for_session(id, |app| app.send_path(path))
    }
    pub fn send_opened_to(
        &mut self,
        id: &str,
        name: String,
        file: std::fs::File,
    ) -> Result<(), String> {
        self.for_session(id, |app| app.send_opened(name, file))
    }

    fn session_snapshots(&self) -> Vec<InitialSession> {
        let mut snapshots: Vec<_> = self
            .chats
            .iter()
            .map(|(id, state)| InitialSession {
                images: self.images.get(id).cloned().unwrap_or_default(),
                id: id.clone(),
                connected: state.session.is_some(),
                history: state.msgs.clone(),
                files: state.transfers.values().cloned().collect(),
            })
            .collect();
        if let Some(id) = &self.peer_pid {
            snapshots.push(InitialSession {
                images: self.images.get(id).cloned().unwrap_or_default(),
                id: id.clone(),
                connected: self.session.is_some(),
                history: self.msgs.clone(),
                files: self.transfers.values().cloned().collect(),
            });
        }
        snapshots.sort_by(|a, b| a.id.cmp(&b.id));
        snapshots
    }

    fn file_owner(&self, id: u64) -> Option<String> {
        if self.transfers.contains_key(&(false, id)) {
            return self.peer_pid.clone();
        }
        self.chats.iter().find_map(|(peer, state)| {
            state
                .transfers
                .contains_key(&(false, id))
                .then(|| peer.clone())
        })
    }

    pub fn initial_state(&self) -> InitialState {
        let prefs: Prefs = std::fs::read(self.dir.join("ui-state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let position = netpos::detect();
        InitialState {
            images: self.images.get(&self.active_session()).cloned().unwrap_or_default(),
            active_session: self.active_session(),
            sessions: self.session_snapshots(),
            mobile: cfg!(target_os = "android"),
            vault_created: self.dir.join("vault.json").is_file(),
            keyfile: prefs.keyfile.unwrap_or_else(|| {
                #[cfg(target_os = "android")]
                {
                    String::new()
                }
                #[cfg(not(target_os = "android"))]
                {
                    dirs::document_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join("spark.keyfile")
                        .display()
                        .to_string()
                }
            }),
            my_id: self.my_pid.clone(),
            position: position.show_hint(),
            position_kind: match position {
                Position::CampusDirect(_) => "campus",
                Position::BehindRouter => "router",
                Position::Offline => "offline",
            }
            .into(),
            history: self.msgs.clone(),
            connected: self.session.is_some() && self.peer_pid.is_some(),
            peer_id: self.peer_pid.clone().unwrap_or_default(),
            files: self.transfers.values().cloned().collect(),
            revision: self.revision,
        }
    }

    pub fn authenticate(
        &mut self,
        passphrase: &str,
        keyfile: &str,
        create: bool,
    ) -> Result<InitialState, String> {
        self.authenticate_with(passphrase, keyfile, create, |dir| {
            let keyfile_path = Path::new(keyfile);
            if create && keyfile_path.exists() {
                return Err("该位置已有文件，请另选一个名字，避免覆盖原来的钥匙".into());
            }
            if create {
                Vault::create(passphrase, keyfile_path, dir).map(|v| v.0)
            } else {
                Vault::unlock(passphrase, keyfile_path, dir)
            }
            .map_err(|e| e.to_string())
        })
    }

    pub(crate) fn authenticate_with(
        &mut self,
        passphrase: &str,
        keyfile: &str,
        create: bool,
        load: impl FnOnce(&Path) -> Result<Vault, String>,
    ) -> Result<InitialState, String> {
        if self.vault.is_some() {
            return Err("已经解锁，请先锁定再重新解锁".into());
        }
        if create && passphrase.chars().count() < 6 {
            return Err("口令至少需要 6 个字符".into());
        }
        if keyfile.trim().is_empty() {
            return Err("请先选择钥匙文件".into());
        }
        let v = load(&self.dir)?;
        let prefs = Prefs {
            vault_created: true,
            keyfile: Some(keyfile.to_owned()),
        };
        let bytes = serde_json::to_vec_pretty(&prefs).map_err(|e| e.to_string())?;
        if let Err(e) = std::fs::write(self.dir.join("ui-state.json"), bytes) {
            self.errors.push(UiEvent::Error(format!(
                "已经解锁，但记住钥匙文件位置失败：{e}。下次打开时请重新选择文件。"
            )));
        }
        self.open_image_cache(&v)?;
        self.my_pid = v.peer_id();
        let (cmd, evt) = net::spawn_multi(v.ed25519_seed(), v.x25519_pub());
        self.multi = true;
        self.net_cmd = Some(cmd);
        self.net_evt = Some(evt);
        self.vault = Some(v);
        self.pos_sent = false;
        Ok(self.initial_state())
    }

    pub(crate) fn event_revision(&self) -> u64 {
        self.revision
    }

    pub fn lock(&mut self) {
        self.image_cache = None;
        self.images.clear();
        self.chats.clear();
        self.multi = false;
        if let Some(tx) = self.net_cmd.take() {
            let _ = tx.send(NetCmd::Shutdown);
        }
        self.net_evt = None;
        self.session = None;
        self.vault = None;
        self.peer_pid = None;
        self.peer_key = None;
        self.msgs.clear();
        self.clear_early();
        self.files.clear();
        self.transfers.clear();
        self.incoming_ids.clear();
        self.errors.clear();
        self.my_pid.clear();
        self.pos_sent = false;
    }

    fn cmd(&self) -> Result<&Sender<NetCmd>, String> {
        self.net_cmd
            .as_ref()
            .ok_or_else(|| "网络尚未启动，请先解锁".into())
    }
    fn send(&self, command: NetCmd) -> Result<(), String> {
        let command = if self.multi
            && !matches!(
                &command,
                NetCmd::StartListen | NetCmd::Dial(_) | NetCmd::Shutdown
            ) {
            NetCmd::To {
                peer_id: self.peer_pid.clone().ok_or("尚未选择会话")?,
                command: Box::new(command),
            }
        } else {
            command
        };
        self.cmd()?
            .send(command)
            .map_err(|_| "网络线程已停止，请锁定后重新解锁".into())
    }
    pub fn listen(&self) -> Result<(), String> {
        self.send(NetCmd::StartListen)
    }
    pub fn connect(&self, code: &str) -> Result<(), String> {
        if code.trim().is_empty() {
            return Err("请粘贴对方发来的连接码".into());
        }
        if code.len() > 4096 {
            return Err("连接码太长，请重新复制".into());
        }
        self.send(NetCmd::Dial(code.to_owned()))
    }

    pub fn text(&mut self, text: &str) -> Result<UiMsg, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("消息不能为空".into());
        }
        if text.len() > MAX_TEXT {
            return Err("消息过长，请改为发送文件（文字最多 1 MB）".into());
        }
        let v = self.vault.as_ref().ok_or("请先解锁")?;
        let s = self.session.as_ref().ok_or("尚未建立聊天，请先连接对方")?;
        let ts = now_ms();
        v.store_msg(s, true, ts, text)
            .map_err(|e| format!("消息没有保存，未发送：{e}"))?;
        self.send(NetCmd::SendText(text.into()))
            .map_err(|e| format!("消息已保存在本机，但没有发送：{e}"))?;
        let m = UiMsg {
            own: true,
            ts,
            text: text.into(),
        };
        self.msgs.push(m.clone());
        Ok(m)
    }

    pub fn send_path(&mut self, path: &Path) -> Result<(), String> {
        if self.session.is_none() {
            return Err("请先连接对方再发送文件".into());
        }
        if !path.is_file() {
            return Err("选择的文件不存在，或它是一个文件夹".into());
        }
        self.send(NetCmd::SendFile(path.to_path_buf()))
    }
    pub(crate) fn send_opened(&self, name: String, file: std::fs::File) -> Result<(), String> {
        if self.session.is_none() {
            return Err("请先连接对方再发送文件".into());
        }
        self.send(NetCmd::SendOpenedFile { name, file })
    }
    fn file_transfers(&self, id: u64) -> &HashMap<(bool, u64), InitialFile> {
        self.chats
            .values()
            .find(|state| state.transfers.contains_key(&(false, id)))
            .map(|state| &state.transfers)
            .unwrap_or(&self.transfers)
    }
    fn file_locations(&self, id: u64) -> &HashMap<u64, String> {
        self.chats
            .values()
            .find(|state| state.transfers.contains_key(&(false, id)))
            .map(|state| &state.files)
            .unwrap_or(&self.files)
    }
    pub fn received_name(&self, id: u64) -> Option<String> {
        self.file_transfers(id)
            .get(&(false, id))
            .filter(|f| !f.complete && !f.accepted && f.failed.is_empty())
            .map(|f| f.name.clone())
    }
    pub fn receive_size(&self, id: u64) -> Result<u64, String> {
        self.file_transfers(id)
            .get(&(false, id))
            .filter(|f| !f.complete && !f.accepted && f.failed.is_empty())
            .map(|f| f.size)
            .ok_or("接收请求已失效，请让对方重新发送".into())
    }
    pub fn saved_location(&self, id: u64) -> Result<String, String> {
        if !self
            .file_transfers(id)
            .get(&(false, id))
            .is_some_and(|f| f.complete)
        {
            return Err("文件尚未接收完成".into());
        }
        self.file_locations(id)
            .get(&id)
            .cloned()
            .ok_or("保存位置已失效".into())
    }
    pub fn check_receive_location(&self, id: u64, location: &str) -> Result<u64, String> {
        if self
            .files
            .iter()
            .chain(self.chats.values().flat_map(|state| state.files.iter()))
            .any(|(other, saved)| {
            *other != id
                && saved == location
                && self
                        .file_transfers(*other)
                    .get(&(false, *other))
                    .is_some_and(|f| !f.complete)
            })
        {
            return Err("该位置已有文件正在接收，请选择不同文件名".into());
        }
        self.receive_size(id)
    }
    pub fn accept_received(
        &mut self,
        id: u64,
        target: crate::transfer::ReceiveTarget,
        location: String,
    ) -> Result<(), String> {
        if let Some(owner) = self.file_owner(id) {
            if self.peer_pid.as_deref() != Some(&owner) {
                return self.for_session(&owner, |app| app.accept_received(id, target, location));
            }
        }
        self.check_receive_location(id, &location)?;
        let wire = self
            .incoming_ids
            .iter()
            .find_map(|(&wire, &local)| (local == id).then_some(wire))
            .ok_or("接收请求已失效")?;
        self.send(NetCmd::AcceptFile { id: wire, target })?;
        self.files.insert(id, location);
        self.transfers.get_mut(&(false, id)).unwrap().accepted = true;
        self.errors.push(UiEvent::FileAccepted { id });
        Ok(())
    }
    pub fn cancel_received(&mut self, id: u64) -> Result<(), String> {
        if let Some(owner) = self.file_owner(id) {
            if self.peer_pid.as_deref() != Some(&owner) {
                return self.for_session(&owner, |app| app.cancel_received(id));
            }
        }
        let wire = self
            .incoming_ids
            .iter()
            .find_map(|(&wire, &local)| (local == id).then_some(wire))
            .ok_or("接收请求已结束")?;
        self.send(NetCmd::RejectFile { id: wire })
    }
    pub fn save_received(&mut self, id: u64, path: &Path) -> Result<(), String> {
        let size = self.check_receive_location(id, &path.to_string_lossy())?;
        let target = crate::transfer::ReceiveTarget::path(path, size).map_err(|e| e.to_string())?;
        self.accept_received(id, target, path.to_string_lossy().into_owned())
    }

    fn clear_early(&mut self) {
        self.early_msgs.clear();
        self.early_overflowed = false;
    }

    fn receive_text(&mut self, message: UiMsg, out: &mut Vec<UiEvent>) {
        if let (Some(v), Some(session)) = (&self.vault, &self.session) {
            if let Err(e) = v.store_msg(session, false, message.ts, &message.text) {
                out.push(UiEvent::Error(format!("收到消息，但本机保存失败：{e}")));
            }
            out.push(UiEvent::Msg(message.text.clone()));
            self.msgs.push(message);
        }
    }

    fn open_peer(&mut self, key: [u8; 32]) -> Result<Vec<UiMsg>, String> {
        let pid = self.peer_pid.as_ref().ok_or("收到公钥时还没有对方身份")?;
        let v = self.vault.as_ref().ok_or("钥匙库已锁定")?;
        let raw = std::fs::read(self.dir.join("vault.json")).map_err(|e| e.to_string())?;
        let saved: serde_json::Value = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        let registry = &saved["peer_registry"];
        let legacy: String = pid
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        if let Some(previous) = registry[pid]
            .as_str()
            .or_else(|| registry[&legacy].as_str())
        {
            if previous != hex {
                return Err(
                    "对方的聊天公钥发生变化。为保护历史记录，已停止此会话，请当面确认身份。".into(),
                );
            }
        }
        v.register_peer(pid, &key).map_err(|e| e.to_string())?;
        // 旧客户端只存身份末 8 位；同一公钥的密文可保留原件并复制到完整身份路径。
        if registry[&legacy].as_str() == Some(&hex) {
            use sha2::{Digest, Sha256};
            let msg_path = |id: &str| {
                let fp: String = Sha256::digest(id.as_bytes())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                self.dir.join(format!("msgs-{}.binl", &fp[..16]))
            };
            let old = msg_path(&legacy);
            let new = msg_path(pid);
            if old.exists() && !new.exists() {
                std::fs::copy(old, new).map_err(|e| format!("读取旧聊天记录失败：{e}"))?;
            }
        }
        let session = v.open_session(pid).map_err(|e| e.to_string())?;
        let history = v
            .load_msgs(&session)
            .map_err(|e| format!("聊天记录无法解密：{e}。原文件已保留。"))?
            .into_iter()
            .map(|m| UiMsg {
                own: m.own,
                ts: m.ts,
                text: m.text,
            })
            .collect::<Vec<_>>();
        self.session = Some(session);
        self.peer_key = Some(key);
        self.msgs = history.clone();
        Ok(history)
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for App {
    fn drop(&mut self) {
        self.lock();
    }
}

impl AppHooks for App {
    fn create_vault(&mut self, passphrase: &str, keyfile: &str) -> Result<String, String> {
        self.authenticate(passphrase, keyfile, true)
            .map(|s| s.my_id)
    }
    fn unlock(&mut self, passphrase: &str, keyfile: &str) -> Result<(), String> {
        self.authenticate(passphrase, keyfile, false).map(|_| ())
    }
    fn start_listen(&mut self) {
        if let Err(e) = self.listen() {
            self.errors.push(UiEvent::Error(e));
        }
    }
    fn dial(&mut self, code: &str) {
        if let Err(e) = self.connect(code) {
            self.errors.push(UiEvent::Error(e));
        }
    }
    fn send_text(&mut self, text: &str) {
        if let Err(e) = self.text(text) {
            self.errors.push(UiEvent::Error(e));
        }
    }
    fn send_file(&mut self, path: &str) {
        if let Err(e) = self.send_path(Path::new(path)) {
            self.errors.push(UiEvent::Error(e));
        }
    }
    fn save_file(&mut self, id: u64, to: &str) {
        if let Err(e) = self.save_received(id, Path::new(to)) {
            self.errors.push(UiEvent::Error(e));
        }
    }
    fn poll_events(&mut self) -> Vec<UiEvent> {
        let mut out = std::mem::take(&mut self.errors);
        if self.vault.is_some() && !self.pos_sent {
            let p = netpos::detect();
            let kind = match p {
                Position::CampusDirect(_) => NetPosKind::CampusDirect,
                Position::BehindRouter => NetPosKind::BehindRouter,
                Position::Offline => NetPosKind::Offline,
            };
            out.push(UiEvent::NetPositionKind {
                text: p.show_hint(),
                kind,
            });
            self.pos_sent = true;
        }
        loop {
            let event = match self.net_evt.as_ref().map(|rx| rx.try_recv()) {
                Some(Ok(ev)) => ev,
                Some(Err(TryRecvError::Disconnected)) => {
                    self.clear_early();
                    self.session = None;
                    self.net_evt = None;
                    self.net_cmd = None;
                    for (peer, state) in &mut self.chats {
                        state.session = None;
                        state.early_msgs.clear();
                        state.incoming_ids.clear();
                        out.push(UiEvent::SessionEvent { session_id: peer.clone(), event: Box::new(UiEvent::Disconnected("网络线程已停止，请锁定后重新解锁".into())) });
                    }
                    let error = UiEvent::Disconnected("网络线程已停止，请锁定后重新解锁".into());
                    out.push(if self.multi { UiEvent::SessionEvent { session_id: self.active_session(), event: Box::new(error) } } else { error });
                    break;
                }
                _ => break,
            };
            let previous = self.active_session();
            let (source, event) = match event {
                NetEvent::PeerEvent { peer_id, event } => {
                    self.multi = true;
                    self.activate(&peer_id);
                    (Some(peer_id), *event)
                }
                other => (None, other),
            };
            let first = out.len();
            'event: {
            match event {
                    NetEvent::PeerEvent { .. } => unreachable!("nested peer event"),
                NetEvent::ImageDelivered => {},
                NetEvent::ImageIncoming { id, bytes } => {
                    let result = if self.session.is_some() {
                        self.store_image(&self.active_session(), &bytes, false)
                    } else { Err("聊天身份尚未验证".into()) };
                    let reply = result.as_ref().map(|_| ()).map_err(Clone::clone);
                    let _ = self.send(NetCmd::ImageResult { id, result: reply });
                    match result { Ok(image) => out.push(UiEvent::Image(image)), Err(error) => out.push(UiEvent::Error(format!("图片接收失败：{error}"))) }
                }
                NetEvent::ListenCode { code, verify } => {
                    out.push(UiEvent::ListenCode { code, verify })
                }
                NetEvent::Connected(p) => {
                    if self
                        .peer_pid
                        .as_ref()
                        .is_some_and(|previous| previous != &p)
                        && self
                            .transfers
                            .values()
                            .any(|f| !f.outgoing && !f.complete && f.failed.is_empty())
                    {
                            let _ = self.send(NetCmd::Disconnect);
                        self.session = None;
                        self.net_evt = None;
                        self.net_cmd = None;
                        out.push(UiEvent::Disconnected(
                            "新会话未打开：还有上一位好友的附件未保存".into(),
                        ));
                        out.push(UiEvent::Error(
                            "请先保存当前附件，再锁定、重新解锁并连接新好友。原附件仍可保存。"
                                .into(),
                        ));
                            break 'event;
                    }
                    if self
                        .peer_pid
                        .as_ref()
                        .is_some_and(|previous| previous != &p)
                    {
                        self.files.clear();
                        self.transfers.clear();
                    }
                    self.incoming_ids.clear();
                    self.clear_early();
                    self.session = None;
                    self.peer_key = None;
                    self.peer_pid = Some(p.clone());
                    out.push(UiEvent::Connected(p));
                }
                NetEvent::PeerKey { x25519_pub } => {
                    if self.session.is_some() && self.peer_key == Some(x25519_pub) {
                            break 'event;
                    }
                    match self.open_peer(x25519_pub) {
                        Ok(history) => {
                            let _ = self.send(NetCmd::Ready);
                            out.push(UiEvent::History(history));
                            out.push(UiEvent::SessionReady(
                                self.peer_pid.clone().unwrap_or_default(),
                            ));
                            let early = std::mem::take(&mut self.early_msgs);
                            self.early_overflowed = false;
                            for message in early {
                                self.receive_text(message, &mut out);
                            }
                        }
                        Err(e) => {
                            self.clear_early();
                            self.session = None;
                            self.peer_key = None;
                                let _ = self.send(NetCmd::Disconnect);
                            out.push(UiEvent::Disconnected(
                                "会话未能安全打开，请锁定后重新解锁".into(),
                            ));
                            out.push(UiEvent::Error(e));
                        }
                    }
                }
                NetEvent::Disconnected(r) => {
                    for ((outgoing, id), file) in &mut self.transfers {
                        if !file.complete && file.failed.is_empty() {
                            file.failed = "连接中断，请重新发送".into();
                            if !outgoing {
                                self.files.remove(id);
                            }
                        }
                    }
                    self.incoming_ids.clear();
                    self.clear_early();
                    self.session = None;
                    out.push(UiEvent::Disconnected(r));
                }
                NetEvent::Msg(text) => {
                    let message = UiMsg {
                        own: false,
                        ts: now_ms(),
                        text,
                    };
                    if self.session.is_some() {
                        self.receive_text(message, &mut out);
                    } else if self.vault.is_some() && self.peer_pid.is_some() {
                        let bytes: usize = self.early_msgs.iter().map(|m| m.text.len()).sum();
                        if self.early_msgs.len() < 16
                            && message.text.len() <= MAX_TEXT.saturating_sub(bytes)
                        {
                            self.early_msgs.push(message);
                        } else if !self.early_overflowed {
                            self.early_overflowed = true;
                            out.push(UiEvent::Error(
                                    "握手完成前收到的消息过多，超出部分未保存，请让对方重发。"
                                        .into(),
                            ));
                        }
                    }
                }
                NetEvent::FileIncoming { id, name, size } => {
                    if self.transfers.len() >= 35 {
                        let _ = self.send(NetCmd::RejectFile { id });
                            break 'event;
                    }
                    if self.transfers.len() >= 32 {
                        if let Some(oldest) = self
                            .transfers
                            .iter()
                            .filter(|((outgoing, _), f)| {
                                !outgoing && (f.complete || !f.failed.is_empty())
                            })
                            .map(|((_, id), _)| *id)
                            .min()
                        {
                            self.transfers.remove(&(false, oldest));
                            self.files.remove(&oldest);
                        }
                    }
                    // 页面与保存按钮使用本机编号，避免对方重启后重用线上编号覆盖旧附件。
                    let local_id = self.next_received_id;
                    self.next_received_id =
                        self.next_received_id.checked_add(1).expect("附件编号耗尽");
                    self.incoming_ids.insert(id, local_id);
                    self.transfers.insert(
                        (false, local_id),
                        InitialFile {
                            id: local_id.to_string(),
                            name: name.clone(),
                            size,
                            outgoing: false,
                            complete: false,
                            accepted: false,
                            failed: String::new(),
                        },
                    );
                    out.push(UiEvent::FileIncoming {
                        id: local_id,
                        name,
                        size,
                    })
                }
                NetEvent::FileOutgoing { id, name, size } => {
                    self.transfers.retain(|(outgoing, _), _| !outgoing);
                    self.transfers.insert(
                        (true, id),
                        InitialFile {
                            id: id.to_string(),
                            name: name.clone(),
                            size,
                            outgoing: true,
                            complete: false,
                            accepted: false,
                            failed: String::new(),
                        },
                    );
                    out.push(UiEvent::FileOutgoing { id, name, size })
                }
                NetEvent::FileProgress { id, done, total } => {
                    if let Some(&local_id) = self.incoming_ids.get(&id) {
                        out.push(UiEvent::FileProgress {
                            id: local_id,
                            done,
                            total,
                        });
                    }
                }
                NetEvent::FileSendProgress { id, done, total } => {
                    out.push(UiEvent::FileSendProgress { id, done, total })
                }
                NetEvent::FileReady { id, name, .. } => {
                    if let Some(local_id) = self.incoming_ids.remove(&id) {
                        if let Some(file) = self.transfers.get_mut(&(false, local_id)) {
                            file.complete = true;
                        }
                        out.push(UiEvent::FileReady { id: local_id, name });
                    }
                }
                NetEvent::FileFailed {
                    id,
                    outgoing,
                    reason,
                } => {
                    let local = if outgoing {
                        Some(id)
                    } else {
                        self.incoming_ids.remove(&id)
                    };
                    if let Some(id) = local {
                        if !outgoing {
                            self.files.remove(&id);
                        }
                        if let Some(f) = self.transfers.get_mut(&(outgoing, id)) {
                            f.failed = reason.clone();
                        }
                        out.push(UiEvent::FileFailed {
                            id,
                            outgoing,
                            reason,
                        });
                    }
                }
                NetEvent::FileAcked { id } => {
                    self.transfers.remove(&(true, id));
                    out.push(UiEvent::FileAcked { id });
                }
                NetEvent::Error(e) => {
                    self.clear_early();
                    out.push(UiEvent::Error(e));
                }
            }
        }
            if let Some(source) = source {
                for event in &mut out[first..] {
                    let original = std::mem::replace(event, UiEvent::Error(String::new()));
                    *event = UiEvent::SessionEvent {
                        session_id: source.clone(),
                        event: Box::new(original),
                    };
                }
                if !previous.is_empty() {
                    self.activate(&previous);
                }
            }
        }
        if !out.is_empty() {
            if self.multi
                && out.iter().any(|event| match event {
                    UiEvent::SessionEvent { event, .. } => matches!(
                        event.as_ref(),
                        UiEvent::Connected(_) | UiEvent::SessionReady(_) | UiEvent::Disconnected(_)
                    ),
                    _ => false,
                })
            {
                out.push(UiEvent::SessionsChanged(self.session_snapshots()));
            }
            self.revision += 1;
        }
        out
    }
    fn load_history(&mut self) -> Vec<UiMsg> {
        self.msgs.clone()
    }
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_apps_exchange_image_into_encrypted_preview_cache() {
        use std::time::{Duration, Instant};
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        std::env::set_var("SPARK_PORT", port.to_string());
        let tmp = tempfile::tempdir().unwrap();
        let mut a = App::with_dir(tmp.path().join("a"));
        let mut b = App::with_dir(tmp.path().join("b"));
        a.authenticate("test-password", tmp.path().join("a.key").to_str().unwrap(), true).unwrap();
        b.authenticate("test-password", tmp.path().join("b.key").to_str().unwrap(), true).unwrap();
        a.listen().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let code = loop {
            if let Some(code) = a.poll_events().into_iter().find_map(|event| if let UiEvent::ListenCode { code, .. } = event { Some(code) } else { None }) { break code; }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut info = crate::protocol::decode_conn_code(&code).unwrap();
        info.addrs = vec![format!("/ip4/127.0.0.1/tcp/{port}")];
        b.connect(&crate::protocol::encode_conn_code(&info).unwrap()).unwrap();
        while a.session.is_none() || b.session.is_none() {
            a.poll_events(); b.poll_events();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let a_id = a.my_pid.clone(); let b_id = b.my_pid.clone();
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(3, 3).write_to(&mut encoded, image::ImageFormat::Png).unwrap();
        let sent = a.image_to(&b_id, encoded.get_ref()).unwrap();
        b.text_to(&a_id, "parallel text").unwrap();
        while b.images.get(&a_id).is_none_or(Vec::is_empty) || a.msgs.is_empty() {
            a.poll_events(); b.poll_events();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let received = &b.images[&a_id][0];
        assert!(!received.own);
        assert_eq!(a.image_preview(&b_id, &sent.id).unwrap(), b.image_preview(&a_id, &received.id).unwrap());
        assert_eq!(a.msgs.last().unwrap().text, "parallel text");
        a.lock(); b.lock();
    }

    #[test]
    fn image_preview_is_scoped_encrypted_and_restored_after_unlock() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("v");
        let keyfile = tmp.path().join("kf");
        let (vault, _) = Vault::create("test-password", &keyfile, &dir).unwrap();
        let mut app = App::with_dir(dir.clone());
        app.open_image_cache(&vault).unwrap();
        app.vault = Some(vault);
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2).write_to(&mut encoded, image::ImageFormat::Png).unwrap();
        let image = app.store_image("alice", encoded.get_ref(), false).unwrap();
        assert!(!app.image_preview("alice", &image.id).unwrap().is_empty());
        assert!(app.image_preview("bob", &image.id).is_err());
        let disk = std::fs::read(dir.join("private-images/index.enc")).unwrap();
        assert!(!disk.windows(5).any(|window| window == b"alice"));
        app.lock();
        assert!(app.image_preview("alice", &image.id).is_err());
        let vault = Vault::unlock("test-password", &keyfile, &dir).unwrap();
        app.open_image_cache(&vault).unwrap();
        app.vault = Some(vault);
        assert!(app.image_to("alice", encoded.get_ref()).is_err());
        assert!(app.peer_pid.is_none());
        assert!(!app.session_snapshots().iter().any(|session| session.id.is_empty()));
        assert_eq!(app.select_session("alice").unwrap().images[0].id, image.id);
        assert!(!app.image_preview("alice", &image.id).unwrap().is_empty());
        assert!(app.image_to("alice", encoded.get_ref()).is_err(), "offline restored history cannot send");
    }

    #[test]
    fn peer_events_and_targeted_actions_keep_sessions_separate() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("v");
        let (vault, _) = Vault::create("test-password", &tmp.path().join("kf"), &dir).unwrap();
        let mut app = App::with_dir(dir);
        app.vault = Some(vault);
        let (tx, rx) = std::sync::mpsc::channel();
        let (commands, command_rx) = std::sync::mpsc::channel();
        app.net_evt = Some(rx);
        app.net_cmd = Some(commands);
        let emit = |peer: &str, event| {
            tx.send(NetEvent::PeerEvent {
                peer_id: peer.into(),
                event: Box::new(event),
            })
            .unwrap()
        };
        for (peer, secret) in [("alice", [3; 32]), ("bob", [4; 32])] {
            let key =
                x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(secret)).to_bytes();
            emit(peer, NetEvent::Connected(peer.into()));
            emit(peer, NetEvent::PeerKey { x25519_pub: key });
            emit(peer, NetEvent::Msg(format!("from {peer}")));
            emit(
                peer,
                NetEvent::FileIncoming {
                    id: 1,
                    name: format!("{peer}.txt"),
                    size: 3,
                },
            );
        }
        let events = app.poll_events();
        while command_rx.try_recv().is_ok() {}
        assert_eq!(app.active_session(), "alice");
        assert_eq!(app.msgs.len(), 1);
        assert_eq!(app.msgs[0].text, "from alice");
        assert!(events.iter().any(|event| matches!(event, UiEvent::SessionEvent { session_id, event } if session_id == "bob" && matches!(event.as_ref(), UiEvent::Msg(s) if s == "from bob"))));
        let snapshots = app.session_snapshots();
        let bob_file = snapshots.iter().find(|s| s.id == "bob").unwrap().files[0]
            .id
            .parse::<u64>()
            .unwrap();
        let alice_file = snapshots.iter().find(|s| s.id == "alice").unwrap().files[0]
            .id
            .parse::<u64>()
            .unwrap();
        assert_ne!(alice_file, bob_file);
        assert_eq!(app.received_name(bob_file).as_deref(), Some("bob.txt"));
        app.save_received(bob_file, &tmp.path().join("bob.txt"))
            .unwrap();
        assert!(
            matches!(command_rx.recv().unwrap(), NetCmd::To { peer_id, command } if peer_id == "bob" && matches!(*command, NetCmd::AcceptFile { id: 1, .. }))
        );
        assert_eq!(app.active_session(), "alice");
        app.text_to("bob", "reply only to bob").unwrap();
        assert!(
            matches!(command_rx.recv().unwrap(), NetCmd::To { peer_id, command } if peer_id == "bob" && matches!(*command, NetCmd::SendText(ref text) if text == "reply only to bob"))
        );
        assert_eq!(app.msgs.len(), 1);
        assert_eq!(app.select_session("bob").unwrap().history.len(), 2);
        emit("alice", NetEvent::Disconnected("left".into()));
        app.poll_events();
        assert!(app.session.is_some());
        assert!(!app.chats["alice"].session.is_some());
        app.lock();
        assert!(app.chats.is_empty() && app.msgs.is_empty());
    }

    #[test]
    fn receive_requires_acceptance_and_only_opens_completed_selected_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::with_dir(tmp.path().join("v"));
        let (tx, rx) = std::sync::mpsc::channel();
        let (commands, command_rx) = std::sync::mpsc::channel();
        app.net_evt = Some(rx);
        app.net_cmd = Some(commands);
        tx.send(NetEvent::FileIncoming {
            id: 77,
            name: "received.txt".into(),
            size: 3,
        })
        .unwrap();
        app.poll_events();
        let id = app.initial_state().files[0].id.parse().unwrap();
        assert!(app.saved_location(id).is_err());
        let path = tmp.path().join("received.txt");
        app.save_received(id, &path).unwrap();
        assert!(
            app.save_received(id, &path).is_err(),
            "不得重复接受同一请求"
        );
        assert!(matches!(
            command_rx.recv().unwrap(),
            NetCmd::AcceptFile { id: 77, .. }
        ));
        assert!(app.saved_location(id).is_err());
        assert!(app.initial_state().files[0].accepted);
        assert!(!app.initial_state().files[0].complete);
        tx.send(NetEvent::FileReady {
            id: 77,
            name: "received.txt".into(),
            sha256: String::new(),
        })
        .unwrap();
        app.poll_events();
        assert_eq!(app.saved_location(id).unwrap(), path.to_string_lossy());
        assert!(app.initial_state().files[0].complete);
        app.lock();
        assert!(app.saved_location(id).is_err());
    }
    #[test]
    fn cancellation_targets_wire_id_and_removes_open_permission() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::with_dir(tmp.path().join("v"));
        let (tx, rx) = std::sync::mpsc::channel();
        let (commands, command_rx) = std::sync::mpsc::channel();
        app.net_evt = Some(rx);
        app.net_cmd = Some(commands);
        tx.send(NetEvent::FileIncoming {
            id: 77,
            name: "f".into(),
            size: 3,
        })
        .unwrap();
        app.poll_events();
        let id = app.initial_state().files[0].id.parse().unwrap();
        app.cancel_received(id).unwrap();
        assert!(matches!(
            command_rx.recv().unwrap(),
            NetCmd::RejectFile { id: 77 }
        ));
        tx.send(NetEvent::FileFailed {
            id: 77,
            outgoing: false,
            reason: "已取消".into(),
        })
        .unwrap();
        app.poll_events();
        assert!(app.saved_location(id).is_err());
        assert!(!app.initial_state().files[0].failed.is_empty());
    }
    #[test]
    fn switching_peer_removes_saved_file_references_without_deleting_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::with_dir(tmp.path().join("v"));
        let path = tmp.path().join("kept.txt");
        std::fs::write(&path, b"kept").unwrap();
        app.peer_pid = Some("old-peer".into());
        app.files.insert(1, path.to_string_lossy().into_owned());
        app.transfers.insert(
            (false, 1),
            InitialFile {
                id: "1".into(),
                name: "kept.txt".into(),
                size: 4,
                outgoing: false,
                complete: true,
                accepted: true,
                failed: String::new(),
            },
        );
        let (tx, rx) = std::sync::mpsc::channel();
        app.net_evt = Some(rx);
        tx.send(NetEvent::Connected("new-peer".into())).unwrap();
        app.poll_events();
        assert!(app.initial_state().files.is_empty());
        assert!(app.saved_location(1).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"kept");
    }
    #[test]
    fn preference_write_failure_does_not_prevent_unlock() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("v");
        let keyfile = tmp.path().join("kf");
        Vault::create("test-password", &keyfile, &dir).unwrap();
        std::fs::create_dir(dir.join("ui-state.json")).unwrap();
        let mut app = App::with_dir(dir);
        assert!(app
            .authenticate("test-password", keyfile.to_str().unwrap(), false)
            .is_ok());
        assert!(app.vault.is_some());
        assert!(app.poll_events().iter().any(
            |e| matches!(e, UiEvent::Error(message) if message.contains("记住钥匙文件位置失败"))
        ));
    }
    #[test]
    fn lock_releases_network_and_plaintext() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::with_dir(tmp.path().join("v"));
        let (tx, rx) = std::sync::mpsc::channel();
        app.net_cmd = Some(tx);
        app.msgs.push(UiMsg {
            own: true,
            ts: 1,
            text: "secret".into(),
        });
        app.files.insert(1, "selected-file".into());
        app.lock();
        assert!(matches!(rx.recv().unwrap(), NetCmd::Shutdown));
        assert!(app.msgs.is_empty() && app.files.is_empty() && app.net_evt.is_none());
    }
    #[test]
    fn text_requires_session_and_caps_size() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::with_dir(tmp.path().join("v"));
        assert!(app.text("hello").is_err());
        assert!(app.text(&"x".repeat(MAX_TEXT + 1)).is_err());
        assert!(app.msgs.is_empty());
    }
    #[test]
    fn tail_id_history_migrates_without_removing_original() {
        use sha2::{Digest, Sha256};
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("v");
        let (vault, _) = Vault::create("test-password", &tmp.path().join("kf"), &dir).unwrap();
        let key =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from([3; 32])).to_bytes();
        let full = "12D3KooWLongIdentity12345678";
        let legacy = "12345678";
        vault.register_peer(legacy, &key).unwrap();
        let session = vault.open_session(legacy).unwrap();
        vault
            .store_msg(&session, true, 1234, "legacy history")
            .unwrap();
        let fp: String = Sha256::digest(legacy.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let old = dir.join(format!("msgs-{}.binl", &fp[..16]));
        let mut app = App::with_dir(dir);
        app.vault = Some(vault);
        app.peer_pid = Some(full.into());
        let h = app.open_peer(key).unwrap();
        assert_eq!(h[0].text, "legacy history");
        assert!(old.exists());
        assert!(app.open_peer([4; 32]).is_err());
    }
    #[test]
    fn history_is_emitted_and_key_change_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("v");
        let (vault, _) = Vault::create("test-password", &tmp.path().join("kf"), &dir).unwrap();
        let key =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from([3; 32])).to_bytes();
        vault.register_peer("peer", &key).unwrap();
        let session = vault.open_session("peer").unwrap();
        vault.store_msg(&session, true, 1234, "history").unwrap();
        let mut app = App::with_dir(dir);
        app.vault = Some(vault);
        app.peer_pid = Some("peer".into());
        let (tx, rx) = std::sync::mpsc::channel();
        app.net_evt = Some(rx);
        tx.send(NetEvent::PeerKey { x25519_pub: key }).unwrap();
        let events = app.poll_events();
        assert!(events
            .iter()
            .any(|e| matches!(e,UiEvent::History(h) if h[0].text=="history")));
        assert!(events.iter().any(|e| matches!(e, UiEvent::SessionReady(_))));
        assert!(app.open_peer([4; 32]).is_err());
    }
    #[test]
    fn early_messages_wait_for_history_then_are_saved_and_displayed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("v");
        let (vault, _) = Vault::create("test-password", &tmp.path().join("kf"), &dir).unwrap();
        let key =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from([3; 32])).to_bytes();
        vault.register_peer("peer", &key).unwrap();
        let session = vault.open_session("peer").unwrap();
        vault
            .store_msg(&session, true, 1234, "old history")
            .unwrap();
        let mut app = App::with_dir(dir);
        app.vault = Some(vault);
        let (tx, rx) = std::sync::mpsc::channel();
        app.net_evt = Some(rx);
        tx.send(NetEvent::Connected("peer".into())).unwrap();
        tx.send(NetEvent::Msg("early greeting".into())).unwrap();
        let before = app.poll_events();
        assert!(!before.iter().any(|e| matches!(e, UiEvent::Msg(_))));
        assert!(app.msgs.is_empty());
        assert_eq!(app.early_msgs.len(), 1);
        tx.send(NetEvent::PeerKey { x25519_pub: key }).unwrap();
        let after = app.poll_events();
        assert!(matches!(&after[0],UiEvent::History(h) if h.len()==1&&h[0].text=="old history"));
        assert!(matches!(&after[1], UiEvent::SessionReady(_)));
        assert!(matches!(&after[2],UiEvent::Msg(t) if t=="early greeting"));
        assert!(app.early_msgs.is_empty());
        let saved = app
            .vault
            .as_ref()
            .unwrap()
            .load_msgs(app.session.as_ref().unwrap())
            .unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[1].text, "early greeting");
        assert_eq!(app.msgs[1].text, "early greeting");
    }

    #[test]
    fn early_message_buffer_has_count_and_byte_limits_and_resets() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("v");
        let (vault, _) = Vault::create("test-password", &tmp.path().join("kf"), &dir).unwrap();
        let mut app = App::with_dir(dir);
        app.vault = Some(vault);
        let (tx, rx) = std::sync::mpsc::channel();
        app.net_evt = Some(rx);
        tx.send(NetEvent::Connected("peer".into())).unwrap();
        for _ in 0..20 {
            tx.send(NetEvent::Msg("hello".into())).unwrap();
        }
        let events = app.poll_events();
        assert_eq!(app.early_msgs.len(), 16);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, UiEvent::Error(_)))
                .count(),
            1
        );
        tx.send(NetEvent::Disconnected("offline".into())).unwrap();
        app.poll_events();
        assert!(app.early_msgs.is_empty() && !app.early_overflowed);
        tx.send(NetEvent::Connected("peer".into())).unwrap();
        tx.send(NetEvent::Msg("x".repeat(MAX_TEXT))).unwrap();
        tx.send(NetEvent::Msg("overflow".into())).unwrap();
        app.poll_events();
        assert_eq!(app.early_msgs.len(), 1);
        assert!(app.early_overflowed);
        app.lock();
        assert!(app.early_msgs.is_empty() && !app.early_overflowed);
    }
}
