use crate::crypt;
use bytes::{Bytes, BytesMut};
use enum_variant_type::EnumVariantType;
use rand::prelude::*;
use serde::{Deserialize, Serialize};
/// Struct that could be any kind of message.
#[derive(Clone, Debug, Serialize, Deserialize, EnumVariantType)]
pub enum Message {
    /// Frame sent from client to server when opening a connection. This is always globally encrypted.
    #[evt(derive(Clone, Debug))]
    ClientHello {
        long_pk: x25519_dalek::PublicKey,
        eph_pk: x25519_dalek::PublicKey,
        version: u64,
    },
    /// Frame sent from server to client to give a cookie for finally opening a connection.
    #[evt(derive(Clone, Debug))]
    ServerHello {
        long_pk: x25519_dalek::PublicKey,
        eph_pk: x25519_dalek::PublicKey,
        /// This value includes all the info required to reconstruct a session, encrypted under a secret key only the server knows.
        cookie: Bytes,
    },

    /// Frame sent from client to server to either signal roaming, or complete an initial handshake. This is globally encrypted.
    /// Clients should send a ClientResume every time they suspect that their IP has changed.
    #[evt(derive(Clone, Debug))]
    ClientResume { cookie: Bytes },

    /// Frame sent as an application-layer message. This is always encrypted with a per-session key.
    #[evt(derive(Clone, Debug))]
    DataFrame {
        /// Strictly incrementing counter of frames. Must never repeat.
        frame_no: u64,
        /// Strictly incrementing counter of runs
        run_no: u64,
        /// Length of current run, in wrapped payload bytes
        run_len: u32,
        /// Highest delivered frame
        high_recv_frame_no: u64,
        /// Total delivered frames
        total_recv_frames: u64,
        /// Body.
        body: Bytes,
    },
}
