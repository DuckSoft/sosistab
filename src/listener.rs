use crate::session::{Session, SessionConfig};
use crate::*;
use async_channel::{Receiver, Sender};
use async_dup::Arc;
use async_lock::Lock;
use bytes::Bytes;
use msg::HandshakeFrame::*;
use rand::prelude::*;
use serde::{Deserialize, Serialize};
use smol::Async;
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

pub struct Listener {
    accepted: Receiver<Session>,
    _task: smol::Task<std::io::Result<()>>,
}

impl Listener {
    /// Accepts a session. This function must be repeatedly called for the entire Listener to make any progress.
    pub async fn accept_session(&self) -> Option<Session> {
        self.accepted.recv().await.ok()
    }
    /// Creates a new listener given the parameters.
    pub fn listen(socket: UdpSocket, long_sk: x25519_dalek::StaticSecret) -> Self {
        let socket = Async::new(socket).unwrap();
        let cookie = crypt::Cookie::new((&long_sk).into());
        let (send, recv) = async_channel::unbounded();
        let task = runtime::spawn(
            ListenerActor {
                socket,
                cookie,
                long_sk,
            }
            .run(send),
        );
        Listener {
            accepted: recv,
            _task: task,
        }
    }
}

struct ListenerActor {
    socket: Async<UdpSocket>,
    cookie: crypt::Cookie,
    long_sk: x25519_dalek::StaticSecret,
}
impl ListenerActor {
    #[allow(clippy::mutable_key_type)]
    async fn run(self, accepted: Sender<Session>) -> std::io::Result<()> {
        let mut addr_to_session: HashMap<
            SocketAddr,
            (Sender<msg::DataFrame>, crypt::StdAEAD, Lock<SocketAddr>),
        > = HashMap::new();
        let mut token_to_addr: HashMap<Bytes, SocketAddr> = HashMap::new();
        let token_key = {
            let mut buf = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut buf);
            buf
        };

        let socket = Arc::new(self.socket);

        let mut buffer = [0u8; 2048];
        loop {
            let (n, addr) = socket.recv_from(&mut buffer).await?;
            let buffer = &buffer[..n];
            // first we attempt to map this to an existing session
            if let Some((sess, sess_crypt, _)) = addr_to_session.get(&addr) {
                // try feeding it into the session
                if let Some(dframe) = sess_crypt.pad_decrypt::<msg::DataFrame>(buffer) {
                    drop(sess.try_send(dframe));
                    continue;
                }
            }
            // we know it's not part of an existing session then. we decrypt it under the current key
            // TODO replay protection
            let s2c_key = self.cookie.generate_s2c().next().unwrap();
            for possible_key in self.cookie.generate_c2s() {
                let crypter = crypt::StdAEAD::new(&possible_key);
                if let Some(handshake) = crypter.pad_decrypt::<msg::HandshakeFrame>(buffer) {
                    match handshake {
                        ClientHello {
                            long_pk,
                            eph_pk,
                            version,
                        } => {
                            if version != 1 {
                                log::warn!("got packet with incorrect version {}", version);
                                break;
                            }
                            // generate session key
                            let my_eph_sk =
                                x25519_dalek::StaticSecret::new(&mut rand::rngs::OsRng {});
                            let token = TokenInfo {
                                sess_key: crypt::triple_ecdh(
                                    &self.long_sk,
                                    &my_eph_sk,
                                    &long_pk,
                                    &eph_pk,
                                )
                                .as_bytes()
                                .to_vec()
                                .into(),
                                init_time_ms: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap()
                                    .as_millis()
                                    as u64,
                            }
                            .encrypt(&token_key);
                            let reply = msg::HandshakeFrame::ServerHello {
                                long_pk: (&self.long_sk).into(),
                                eph_pk: (&my_eph_sk).into(),
                                resume_token: token,
                            };
                            let reply = crypt::StdAEAD::new(&s2c_key).pad_encrypt(&reply, 1300);
                            socket.send_to(&reply, addr).await?;
                            log::trace!("replied to ClientHello from {}", addr);
                        }
                        ClientResume { resume_token } => {
                            // first check whether we know about the resume token
                            if let Some(old_addr) = token_to_addr.get_mut(&resume_token) {
                                log::trace!(
                                    "ClientResume from {} corresponding to old {}",
                                    addr,
                                    old_addr
                                );
                                if *old_addr != addr {
                                    let (sess, aead, locked_addr) =
                                        addr_to_session.remove(old_addr).unwrap();
                                    *locked_addr.lock().await = addr;
                                    addr_to_session.insert(addr, (sess, aead, locked_addr));
                                    *old_addr = addr;
                                }
                            } else {
                                log::trace!("ClientResume from {} is new!", addr);
                                let tokinfo = TokenInfo::decrypt(&token_key, &resume_token);
                                if let Some(tokinfo) = tokinfo {
                                    let up_key =
                                        blake3::keyed_hash(crypt::UP_KEY, &tokinfo.sess_key);
                                    let dn_key =
                                        blake3::keyed_hash(crypt::DN_KEY, &tokinfo.sess_key);
                                    let up_aead = crypt::StdAEAD::new(up_key.as_bytes());
                                    let dn_aead = crypt::StdAEAD::new(dn_key.as_bytes());
                                    let socket = socket.clone();
                                    let (session_input, session_input_recv) =
                                        async_channel::bounded(100);
                                    // create session
                                    let (session_output_send, session_output_recv) =
                                        async_channel::bounded::<msg::DataFrame>(100);
                                    // send for poll
                                    let locked_addr = Lock::new(addr);
                                    let output_poller = {
                                        let locked_addr = locked_addr.clone();
                                        runtime::spawn(async move {
                                            loop {
                                                match session_output_recv.recv().await {
                                                    Ok(df) => {
                                                        let enc = dn_aead.pad_encrypt(&df, 1300);
                                                        drop(
                                                            socket
                                                                .send_to(
                                                                    &enc,
                                                                    *locked_addr.lock().await,
                                                                )
                                                                .await,
                                                        );
                                                    }
                                                    Err(_) => smol::future::pending::<()>().await,
                                                }
                                            }
                                        })
                                    };
                                    let mut session = Session::new(SessionConfig {
                                        latency: Duration::from_millis(5),
                                        target_loss: 0.005,
                                        send_frame: session_output_send,
                                        recv_frame: session_input_recv,
                                    });
                                    session.on_drop(move || {
                                        drop(output_poller);
                                    });
                                    // spawn a task that writes to the socket.
                                    addr_to_session
                                        .insert(addr, (session_input, up_aead, locked_addr));
                                    token_to_addr.insert(resume_token, addr);
                                    drop(accepted.send(session).await);
                                } else {
                                    log::warn!("ClientResume from {} can't be decrypted", addr);
                                }
                            }
                        }
                        _ => continue,
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenInfo {
    sess_key: Bytes,
    init_time_ms: u64,
}

impl TokenInfo {
    fn decrypt(key: &[u8], encrypted: &[u8]) -> Option<Self> {
        // first we decrypt
        let crypter = crypt::StdAEAD::new(key);
        let plain = crypter.decrypt(encrypted)?;
        bincode::deserialize(&plain).ok()
    }

    fn encrypt(&self, key: &[u8]) -> Bytes {
        let crypter = crypt::StdAEAD::new(key);
        let mut rng = rand::thread_rng();
        crypter.encrypt(
            &bincode::serialize(self).expect("must serialize"),
            rng.gen(),
        )
    }
}
