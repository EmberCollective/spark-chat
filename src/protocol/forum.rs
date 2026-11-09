//! Forum v1 objects. The caller limits framed bytes before deserializing.
use serde::{Deserialize, Serialize};

pub type Digest = [u8; 32];
pub const MAX_RECORD_BYTES: usize = 32 * 1024;
pub const MAX_BODY_BYTES: usize = 80_000;
pub const MAX_ATTACHMENT_BYTES: u64 = 20_000_000;
pub const MAX_ATTACHMENTS_BYTES: u64 = 30_000_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub hash: Digest,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum GovernanceChange {
    Admit {
        member: Vec<u8>,
        inviter: Vec<u8>,
        expires_at: u64,
    },
    Revoke {
        member: Vec<u8>,
    },
    Checkpoint {
        records: Vec<Digest>,
    },
    Moderate {
        target: Digest,
        hidden: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Action {
    Governance {
        sequence: u64,
        previous: Option<Digest>,
        change: GovernanceChange,
    },
    Content {
        /// None creates a new topic or reply; Some edits that immutable root.
        target: Option<Digest>,
        /// Only a new reply sets this to an existing topic root.
        reply_to: Option<Digest>,
        parents: Vec<Digest>,
        title: String,
        body: Digest,
        attachments: Vec<Attachment>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForumRecord {
    pub network: Digest,
    /// Ed25519 public key encoded using libp2p protobuf encoding.
    pub author: Vec<u8>,
    pub nonce: [u8; 16],
    pub action: Action,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedRecord {
    pub record: ForumRecord,
    pub signature: Vec<u8>,
}

/// Bounded request/response protocol, separate from legacy private chat.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub network: Digest,
    pub query: Query,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Query {
    Offer {
        record: SignedRecord,
        body: Vec<u8>,
    },
    Stored {
        records: Vec<Digest>,
        objects: Vec<Digest>,
    },
    Neighbors,
    Find {
        hash: Digest,
        request_id: [u8; 16],
        hops: u8,
    },
    Chunk {
        hash: Digest,
        offset: u64,
    },
    Hello {
        governance: Vec<SignedRecord>,
    },
    Inventory {
        after: Option<Digest>,
    },
    Records {
        ids: Vec<Digest>,
    },
    Body {
        hash: Digest,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Response {
    Stored,
    Accepted {
        record: Digest,
        body: Digest,
    },
    Neighbors {
        nodes: Vec<NodeAddress>,
    },
    Found {
        hash: Digest,
        nodes: Vec<NodeAddress>,
    },
    Busy,
    Chunk {
        hash: Digest,
        offset: u64,
        total: u64,
        bytes: Option<Vec<u8>>,
    },
    Denied,
    Hello {
        governance: Vec<SignedRecord>,
    },
    Inventory {
        ids: Vec<Digest>,
        more: bool,
    },
    Records {
        records: Vec<SignedRecord>,
    },
    Body {
        hash: Digest,
        bytes: Option<Vec<u8>>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeAddress {
    pub peer: String,
    pub addresses: Vec<String>,
}
