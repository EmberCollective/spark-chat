//! 集成层：密钥留在 Rust，界面只收到可显示的状态与消息。
use crate::net::{self, NetCmd, NetEvent};
use crate::netpos::{self, Position};
use crate::ui::{AppHooks, NetPosKind, UiEvent, UiMsg};
use crate::vault::{Session, Vault};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

const MAX_TEXT: usize = 1024 * 1024;

#[derive(Clone, Serialize)]
pub struct InitialFile {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub outgoing: bool,
    pub complete: bool,
    pub accepted: bool,
    pub failed: String,
}

#[derive(Serialize)]
pub struct InitialState {
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

    pub fn initial_state(&self) -> InitialState {
        let prefs: Prefs = std::fs::read(self.dir.join("ui-state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let position = netpos::detect();
        InitialState {
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
        self.my_pid = v.peer_id();
        let (cmd, evt) = net::spawn(v.ed25519_seed(), v.x25519_pub());
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
    #[cfg(target_os = "android")]
    pub(crate) fn send_opened(&self, name: String, file: std::fs::File) -> Result<(), String> {
        if self.session.is_none() {
            return Err("请先连接对方再发送文件".into());
        }
        self.send(NetCmd::SendOpenedFile { name, file })
    }
    pub fn received_name(&self, id: u64) -> Option<String> {
        self.transfers
            .get(&(false, id))
            .filter(|f| !f.complete && !f.accepted && f.failed.is_empty())
            .map(|f| f.name.clone())
    }
    pub fn receive_size(&self, id: u64) -> Result<u64, String> {
        self.transfers
            .get(&(false, id))
            .filter(|f| !f.complete && !f.accepted && f.failed.is_empty())
            .map(|f| f.size)
            .ok_or("接收请求已失效，请让对方重新发送".into())
    }
    pub fn saved_location(&self, id: u64) -> Result<String, String> {
        if !self.transfers.get(&(false, id)).is_some_and(|f| f.complete) {
            return Err("文件尚未接收完成".into());
        }
        self.files.get(&id).cloned().ok_or("保存位置已失效".into())
    }
    pub fn check_receive_location(&self, id: u64, location: &str) -> Result<u64, String> {
        if self.files.iter().any(|(other, saved)| {
            *other != id
                && saved == location
                && self
                    .transfers
                    .get(&(false, *other))
                    .is_some_and(|f| !f.complete)
        }) {
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
                    out.push(UiEvent::Disconnected(
                        "网络线程已停止，请锁定后重新解锁".into(),
                    ));
                    break;
                }
                _ => break,
            };
            match event {
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
                        let _ = self.send(NetCmd::Shutdown);
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
                        break;
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
                        continue;
                    }
                    match self.open_peer(x25519_pub) {
                        Ok(history) => {
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
                            let _ = self.send(NetCmd::Shutdown);
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
                                "握手完成前收到的消息过多，超出部分未保存，请让对方重发。".into(),
                            ));
                        }
                    }
                }
                NetEvent::FileIncoming { id, name, size } => {
                    if self.transfers.len() >= 35 {
                        let _ = self.send(NetCmd::RejectFile { id });
                        continue;
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
        if !out.is_empty() {
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
