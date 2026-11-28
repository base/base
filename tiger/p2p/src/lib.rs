#![doc = include_str!("../README.md")]

mod noise;
pub use noise::{
    ACT_ONE_LEN, ACT_THREE_LEN, ACT_TWO_LEN, AwaitingActThree, CipherState, HANDSHAKE_VERSION,
    Initiator, LENGTH_HEADER_LEN, MAX_MESSAGE_LEN, NoiseError, PROTOCOL_NAME, Receiver, Responder,
    Sender, Session, TAG_LEN,
};

mod wire;
pub use wire::{
    CHUNK_HEADER_LEN, ChunkError, DisconnectReason, FLAG_BULK, FLAG_END, FLAG_START,
    MAX_CHUNK_DATA, MAX_IDS, MAX_PEER_RECORDS, Message, MessageId, PROTOCOL_VERSION, PeerRecord,
    Reassembler, ResponseCode, START_HEADER_LEN, Status, WireError, chunk,
};

mod table;
pub use table::{
    BUCKET_SIZE, Entry, NEW_BUCKETS, NEW_BUCKETS_PER_SOURCE_GROUP, PeerId, PeerTable,
    TRIED_BUCKETS, group,
};

mod node;
pub use node::{
    BAN_SCORE, Config, ConnId, Direction, Event, MAX_ANCHORS, Network, Node, NotifyKind, Output, Penalty,
    RequestError, RequestId, RequestKind, Rng, Stats, Target, Validation, message_id,
};

mod driver;
pub use driver::{
    FRAME_TIMEOUT, HANDSHAKE_TIMEOUT, Handle, IDLE_TIMEOUT, MAX_PENDING_HANDSHAKES,
    MAX_PENDING_HANDSHAKES_PER_IP, MAX_QUEUED_BULK, MESSAGE_TIMEOUT, MAX_QUEUED_PRIORITY, Traffic, random_secret,
    start,
};
