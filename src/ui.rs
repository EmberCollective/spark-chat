//! Rust 与桌面界面共享的消息契约。界面通过 AppHooks 调用后端。
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize)]
pub struct UiMsg {
    pub own: bool,
    pub ts: i64,
    pub text: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiImage {
    pub id: String,
    pub own: bool,
    pub ts: i64,
    pub mime: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UiFile {
    #[serde(serialize_with = "id_string")]
    pub id: u64,
    pub name: String,
    pub done: u64,
    pub total: u64,
}

pub trait AppHooks {
    fn create_vault(&mut self, passphrase: &str, keyfile: &str) -> Result<String, String>;
    fn unlock(&mut self, passphrase: &str, keyfile: &str) -> Result<(), String>;
    fn start_listen(&mut self);
    fn dial(&mut self, code: &str);
    fn send_text(&mut self, text: &str);
    fn send_file(&mut self, path: &str);
    fn save_file(&mut self, id: u64, to: &str);
    fn poll_events(&mut self) -> Vec<UiEvent>;
    fn load_history(&mut self) -> Vec<UiMsg>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum NetPosKind {
    CampusDirect,
    BehindRouter,
    Offline,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum UiEvent {
    Image(UiImage),
    SessionEvent {
        session_id: String,
        event: Box<UiEvent>,
    },
    SessionsChanged(Vec<crate::app::InitialSession>),
    NetPosition(String),
    NetPositionKind {
        text: String,
        kind: NetPosKind,
    },
    OwnId(String),
    ListenCode {
        code: String,
        verify: String,
    },
    Connected(String),
    SessionReady(String),
    History(Vec<UiMsg>),
    Disconnected(String),
    Msg(String),
    FileIncoming {
        #[serde(serialize_with = "id_string")]
        id: u64,
        name: String,
        size: u64,
    },
    FileOutgoing {
        #[serde(serialize_with = "id_string")]
        id: u64,
        name: String,
        size: u64,
    },
    FileProgress {
        #[serde(serialize_with = "id_string")]
        id: u64,
        done: u64,
        total: u64,
    },
    FileSendProgress {
        #[serde(serialize_with = "id_string")]
        id: u64,
        done: u64,
        total: u64,
    },
    FileReady {
        #[serde(serialize_with = "id_string")]
        id: u64,
        name: String,
    },
    FileAccepted {
        #[serde(serialize_with = "id_string")]
        id: u64,
    },
    FileFailed {
        #[serde(serialize_with = "id_string")]
        id: u64,
        outgoing: bool,
        reason: String,
    },
    FileAcked {
        #[serde(serialize_with = "id_string")]
        id: u64,
    },
    PeerKeyVerified(String),
    Error(String),
}

fn id_string<S: serde::Serializer>(id: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_file_ids_are_exact_json_strings() {
        let event = UiEvent::FileIncoming {
            id: u64::MAX,
            name: "f".into(),
            size: 1,
        };
        let json = serde_json::to_value(event).unwrap();
        assert_eq!(json["data"]["id"], u64::MAX.to_string());
    }
}
