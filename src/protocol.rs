//! Wire protocol shared by all peers on the multicast group.

use serde::{Deserialize, Serialize};

/// Magic bytes so we ignore stray datagrams that aren't ours.
pub const MAGIC: u32 = 0x4C_41_4E_43; // "LANC"

/// Voice is captured/played back at this fixed rate, mono.
pub const SAMPLE_RATE: u32 = 48_000;

/// A single datagram on the wire: a small header followed by a payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub magic: u32,
    /// Random per-process id so we can drop our own looped-back packets.
    pub peer_id: u64,
    pub payload: Payload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Payload {
    /// Announce presence / keepalive.
    Hello { name: String },
    /// Graceful departure.
    Bye { name: String },
    /// Text chat line.
    Text { name: String, body: String },
    /// An emote / action ("/me waves").
    Emote { name: String, action: String },
    /// One frame of mono f32 PCM at SAMPLE_RATE.
    Voice { name: String, seq: u32, pcm: Vec<f32> },
}

impl Envelope {
    pub fn new(peer_id: u64, payload: Payload) -> Self {
        Self { magic: MAGIC, peer_id, payload }
    }

    pub fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("serialize envelope")
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let env: Envelope = postcard::from_bytes(bytes).ok()?;
        if env.magic != MAGIC {
            return None;
        }
        Some(env)
    }
}
