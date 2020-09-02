use crate::*;
use async_channel::{Receiver, Sender};
use async_rwlock::RwLock;
use bytes::Bytes;
use mux::relconn::{RelConn, RelConnBack, RelConnState};
use mux::structs::*;
use rand::prelude::*;
use smol::prelude::*;
use std::collections::HashMap;

pub async fn multiplex(
    session: Session,
    urel_send_recv: Receiver<Bytes>,
    urel_recv_send: Sender<Bytes>,
    conn_open_recv: Receiver<Sender<RelConn>>,
    conn_accept_send: Sender<RelConn>,
) -> anyhow::Result<()> {
    let conn_tab = RwLock::new(ConnTable::default());
    let (glob_send, glob_recv) = async_channel::bounded(10000);
    loop {
        // fires on receiving messages
        let recv_evt = async {
            let msg = session.recv_bytes().await;
            let msg = bincode::deserialize::<Message>(&msg);
            if let Ok(msg) = msg {
                match msg {
                    // unreliable
                    Message::Urel(bts) => {
                        log::trace!("urel recv {}B", bts.len());
                        drop(urel_recv_send.send(bts).await);
                    }
                    // connection opening
                    Message::Rel {
                        kind: RelKind::Syn,
                        stream_id,
                        ..
                    } => {
                        let mut conn_tab = conn_tab.write().await;
                        if conn_tab.get_stream(stream_id).is_some() {
                            log::trace!("syn recv {} REJECT", stream_id);
                            session
                                .send_bytes(
                                    bincode::serialize(&Message::Rel {
                                        kind: RelKind::Rst,
                                        stream_id,
                                        seqno: 0,
                                        payload: Bytes::new(),
                                    })
                                    .unwrap()
                                    .into(),
                                )
                                .await;
                        } else {
                            log::trace!("syn recv {} ACCEPT", stream_id);
                            let (new_conn, new_conn_back) = RelConn::new(
                                RelConnState::SynReceived { stream_id },
                                glob_send.clone(),
                            );
                            // the RelConn itself is responsible for sending the SynAck. Here we just store the connection into the table, accept it, and be done with it.
                            conn_tab.set_stream(stream_id, new_conn_back);
                            drop(conn_accept_send.send(new_conn).await);
                        }
                    }
                    // associated with existing connection
                    Message::Rel {
                        stream_id, kind, ..
                    } => {
                        if let Some(handle) = conn_tab.read().await.get_stream(stream_id) {
                            log::trace!("handing over {:?} to {}", kind, stream_id);
                            handle.process(msg).await
                        } else {
                            log::trace!("discarding {:?} to nonexistent {}", kind, stream_id);
                            if kind != RelKind::Rst {
                                session
                                    .send_bytes(
                                        bincode::serialize(&Message::Rel {
                                            kind: RelKind::Rst,
                                            stream_id,
                                            seqno: 0,
                                            payload: Bytes::new(),
                                        })
                                        .unwrap()
                                        .into(),
                                    )
                                    .await;
                            }
                        }
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        };
        // fires on sending messages
        let send_evt = async {
            let to_send = glob_recv.recv().await?;
            if let Message::Rel {
                stream_id,
                kind: RelKind::Rst,
                ..
            } = &to_send
            {
                log::trace!("RESET intercepted for {}, removing from table!", stream_id);
                conn_tab.write().await.del_stream(*stream_id);
            }
            session
                .send_bytes(bincode::serialize(&to_send).unwrap().into())
                .await;
            Ok::<(), anyhow::Error>(())
        };
        // fires on a new unreliable sending request
        let urel_send_evt = async {
            let to_send = urel_send_recv.recv().await?;
            log::trace!("urel send {}B", to_send.len());
            glob_send.send(Message::Urel(to_send)).await?;
            Ok::<(), anyhow::Error>(())
        };
        // fires on a new stream open request
        let conn_open_evt = async {
            let result_chan = conn_open_recv.recv().await?;
            let stream_id = {
                let mut conn_tab = conn_tab.write().await;
                let stream_id = conn_tab
                    .find_id()
                    .ok_or_else(|| anyhow::anyhow!("ran out of connection ids"))?;
                let (conn, conn_back) = RelConn::new(
                    RelConnState::SynSent {
                        stream_id,
                        tries: 0,
                    },
                    glob_send.clone(),
                );
                drop(result_chan.send(conn).await);
                conn_tab.set_stream(stream_id, conn_back);
                stream_id
            };
            log::trace!("conn open send {}", stream_id);
            drop(
                glob_send
                    .send(Message::Rel {
                        kind: RelKind::Syn,
                        stream_id,
                        seqno: 0,
                        payload: Bytes::new(),
                    })
                    .await,
            );
            Ok::<(), anyhow::Error>(())
        };
        // await on them all
        recv_evt
            .or(send_evt.or(urel_send_evt.or(conn_open_evt)))
            .await?;
    }
}

#[derive(Default)]
struct ConnTable {
    /// Maps IDs to RelConn back handles.
    sid_to_stream: HashMap<u16, RelConnBack>,
}

impl ConnTable {
    fn get_stream(&self, sid: u16) -> Option<&RelConnBack> {
        self.sid_to_stream.get(&sid)
    }

    fn set_stream(&mut self, id: u16, handle: RelConnBack) {
        self.sid_to_stream.insert(id, handle);
    }

    fn del_stream(&mut self, id: u16) {
        self.sid_to_stream.remove(&id);
    }

    fn find_id(&mut self) -> Option<u16> {
        if self.sid_to_stream.len() >= 65535 {
            return None;
        }
        loop {
            let possible_id: u16 = rand::thread_rng().gen();
            if self.sid_to_stream.get(&possible_id).is_none() {
                log::debug!(
                    "found id {} out of {}",
                    possible_id,
                    self.sid_to_stream.len()
                );
                break Some(possible_id);
            }
        }
    }
}
