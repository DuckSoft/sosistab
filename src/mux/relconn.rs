use crate::*;
use async_channel::{Receiver, Sender};
use bytes::Bytes;
use mux::structs::{Message, RelKind, Reorderer, Seqno};
use smol::io::{AsyncRead, AsyncWrite};
use smol::{future::Boxed, prelude::*};
use std::future::Future;
use std::time::{Duration, Instant};

const MSS: usize = 1024;
const MAX_WAIT_SECS: u64 = 60;
const DUPACK_THRESHOLD: usize = 3;

#[derive(Clone)]
pub struct RelConn {
    send_write: Sender<Bytes>,
    recv_read: Receiver<Bytes>,

    read_buffer: Bytes,
}

// impl AsyncRead for RelConn {
//     fn poll_read(
//         self: Pin<&mut Self>,
//         cx: &mut Context<'_>,
//         buf: &mut [u8],
//     ) -> Poll<std::io::Result<usize>> {
//         while self.read_buffer.is_empty() {
//             let lala = self.clone();
//             let recv_fut = lala.recv_raw_bytes().boxed();
//             smol::pin!(recv_fut);
//             match recv_fut.poll(cx) {
//                 Poll::Pending => return Poll::Pending,
//                 Poll::Ready(bts) => self.read_buffer = bts?,
//             }
//         }
//         unimplemented!()
//     }
// }

impl RelConn {
    pub(crate) fn new(state: RelConnState, output: Sender<Message>) -> (Self, RelConnBack) {
        let (send_write, recv_write) = async_channel::bounded(100);
        let (send_read, recv_read) = async_channel::bounded(100);
        let (send_wire_read, recv_wire_read) = async_channel::bounded(10000);
        runtime::spawn(relconn_actor(
            state,
            recv_write,
            send_read,
            recv_wire_read,
            output,
        ))
        .detach();
        (
            RelConn {
                send_write,
                recv_read,

                read_buffer: Bytes::new(),
            },
            RelConnBack { send_wire_read },
        )
    }

    pub async fn send_raw_bytes(&self, bts: Bytes) -> std::io::Result<()> {
        assert!(bts.len() <= MSS);
        self.send_write
            .send(bts)
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e))
    }

    pub async fn recv_raw_bytes(&self) -> std::io::Result<Bytes> {
        self.recv_read
            .recv()
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e))
    }
}

pub(crate) enum RelConnState {
    SynReceived { stream_id: u16 },
    SynSent { stream_id: u16, tries: usize },
    SteadyState { stream_id: u16, conn_vars: ConnVars },
    Reset { stream_id: u16, death: smol::Timer },
}
use std::{
    collections::VecDeque,
    num::{NonZeroUsize, Wrapping},
    pin::Pin,
    task::{Context, Poll},
};
use RelConnState::*;

async fn unwrap_or_sleep<T>(val: Option<T>) -> T {
    match val {
        Some(val) => val,
        None => smol::future::pending::<T>().await,
    }
}

async fn relconn_actor(
    mut state: RelConnState,
    recv_write: Receiver<Bytes>,
    send_read: Sender<Bytes>,
    recv_wire_read: Receiver<Message>,
    send_wire_write: Sender<Message>,
) -> anyhow::Result<()> {
    // match on our current state repeatedly
    #[derive(Debug, Clone)]
    enum Evt {
        Rto,
        DelayedAck,
        NewWrite(Bytes),
        NewPkt(Message),
    }
    let _guard = scopeguard::guard((), |_| {
        println!("Hello Scope Exit!");
    });
    let transmit = |msg| async {
        drop(send_wire_write.send(msg).await);
    };
    loop {
        state = match state {
            SynReceived { stream_id } => {
                log::trace!("C={} SynReceived, sending SYN-ACK", stream_id);
                // send a synack
                transmit(Message::Rel {
                    kind: RelKind::SynAck,
                    stream_id,
                    seqno: 0,
                    ack_seqno: 0,
                    payload: Bytes::new(),
                })
                .await;
                SteadyState {
                    stream_id,
                    conn_vars: ConnVars::default(),
                }
            }
            SynSent { stream_id, tries } => {
                let wait_interval = 2u64.saturating_pow(tries as u32);
                log::trace!("C={} SynSent, tried {} times", stream_id, tries);
                if wait_interval > MAX_WAIT_SECS {
                    anyhow::bail!("timeout in SynSent");
                }
                let synack_evt = async {
                    loop {
                        match dbg!(recv_wire_read.recv().await?) {
                            Message::Rel { .. } => return Ok::<_, anyhow::Error>(true),
                            _ => continue,
                        }
                    }
                };
                let success = synack_evt
                    .or(async {
                        smol::Timer::new(Duration::from_secs(wait_interval as u64)).await;
                        Ok(false)
                    })
                    .await?;
                if success {
                    log::trace!("C={} SynSent got SYN-ACK", stream_id);
                    SteadyState {
                        stream_id,
                        conn_vars: ConnVars::default(),
                    }
                } else {
                    log::trace!("C={} SynSent timed out", stream_id);
                    SynSent {
                        stream_id,
                        tries: tries + 1,
                    }
                }
            }
            SteadyState {
                stream_id,
                mut conn_vars,
            } => {
                let event = {
                    let writeable = conn_vars.inflight.len() <= conn_vars.cwnd as usize;
                    let rto_timer = &mut conn_vars.rto_timer;
                    let delayed_ack_timer = &mut conn_vars.delayed_ack_timer;
                    let rto_timeout = async {
                        if let Some(rto) = rto_timer {
                            rto.await;
                            Ok::<Evt, anyhow::Error>(Evt::Rto)
                        } else {
                            Ok(smol::future::pending().await)
                        }
                    };
                    let ack_timeout = async {
                        if let Some(rto) = delayed_ack_timer {
                            rto.await;
                            Ok::<Evt, anyhow::Error>(Evt::DelayedAck)
                        } else {
                            Ok(smol::future::pending().await)
                        }
                    };
                    let new_write = async {
                        if writeable {
                            let to_write = recv_write.recv().await?;
                            Ok::<Evt, anyhow::Error>(Evt::NewWrite(to_write))
                        } else {
                            Ok(smol::future::pending().await)
                        }
                    };
                    let new_pkt = async {
                        Ok::<Evt, anyhow::Error>(Evt::NewPkt(recv_wire_read.recv().await?))
                    };
                    rto_timeout.or(ack_timeout).or(new_write).or(new_pkt).await
                };
                match event {
                    Ok(Evt::Rto) => {
                        // retransmit first unacknowledged packet
                        assert!(!conn_vars.inflight.is_empty());
                        conn_vars.congestion_rto();
                        for (seqno, pkt) in conn_vars.inflight.iter().take(1) {
                            transmit(pkt.clone()).await;
                        }
                        // reset RTO timer
                        conn_vars.rto_duration += conn_vars.rto_duration / 3;
                        log::trace!("RTO; duration now {}ms", conn_vars.rto_duration.as_millis());
                        conn_vars.rto_timer = Some(smol::Timer::new(conn_vars.rto_duration));
                        // new state
                        SteadyState {
                            stream_id,
                            conn_vars,
                        }
                    }
                    Ok(Evt::DelayedAck) => {
                        transmit(Message::Rel {
                            kind: RelKind::Data,
                            ack_seqno: conn_vars.lowest_unseen,
                            seqno: 0,
                            stream_id,
                            payload: Bytes::new(),
                        })
                        .await;
                        log::trace!("delayed ACK timer fired!");
                        conn_vars.delayed_ack_timer = None;
                        SteadyState {
                            stream_id,
                            conn_vars,
                        }
                    }
                    Ok(Evt::NewPkt(Message::Rel {
                        kind: RelKind::Rst,
                        stream_id,
                        ..
                    })) => Reset {
                        stream_id,
                        death: smol::Timer::new(Duration::from_secs(60)),
                    },
                    Ok(Evt::NewPkt(Message::Rel {
                        kind: RelKind::Data,
                        seqno,
                        ack_seqno,
                        payload,
                        stream_id,
                    })) => {
                        log::trace!("new data pkt with seqno={}, ack_seqno={}", seqno, ack_seqno);
                        // process ack
                        while !conn_vars.inflight.is_empty() && conn_vars.inflight[0].0 < ack_seqno
                        {
                            log::trace!(
                                "ack_seqno={} acknowledges {}",
                                ack_seqno,
                                conn_vars.inflight[0].0
                            );
                            conn_vars.inflight.pop_front();
                            conn_vars.congestion_ack();
                            conn_vars.rto_duration = Duration::from_secs(1);
                        }
                        if conn_vars.delayed_ack_timer.is_none() {
                            log::trace!("scheduling delayed ACK");
                            conn_vars.delayed_ack_timer =
                                Some(smol::Timer::new(Duration::from_millis(10)));
                        }
                        if conn_vars.inflight.is_empty() {
                            conn_vars.rto_timer = None
                        }
                        if !payload.is_empty() {
                            if seqno >= conn_vars.lowest_unseen {
                                log::trace!("insert {}", seqno);
                                conn_vars.reorderer.insert(
                                    seqno,
                                    Message::Rel {
                                        kind: RelKind::Data,
                                        seqno,
                                        ack_seqno,
                                        payload,
                                        stream_id,
                                    },
                                );
                            }
                            for out in conn_vars.reorderer.take() {
                                if let Message::Rel { payload, seqno, .. } = out {
                                    log::trace!("taking seqno {}", seqno);
                                    drop(send_read.send(payload).await);
                                    conn_vars.lowest_unseen = seqno + 1;
                                }
                            }
                        }
                        // process dupack
                        if conn_vars.dupack_seqno == ack_seqno {
                            conn_vars.dupack_count += 1;
                            if conn_vars.dupack_count >= DUPACK_THRESHOLD
                                && !conn_vars.inflight.is_empty()
                            {
                                log::trace!("fast retransmit {}", ack_seqno);
                                conn_vars.dupack_count = 0;
                                conn_vars.congestion_fast();
                                for (_, pkt) in conn_vars.inflight.iter().take(1) {
                                    transmit(pkt.clone()).await;
                                }
                            }
                        } else {
                            conn_vars.dupack_count = 0;
                            conn_vars.dupack_seqno = ack_seqno;
                        }
                        SteadyState {
                            stream_id,
                            conn_vars,
                        }
                    }
                    Ok(Evt::NewWrite(b)) => {
                        let seqno = conn_vars.next_free_seqno;
                        conn_vars.next_free_seqno += 1;
                        let msg = Message::Rel {
                            kind: RelKind::Data,
                            stream_id,
                            seqno,
                            ack_seqno: conn_vars.lowest_unseen,
                            payload: b,
                        };
                        // put msg into inflight
                        conn_vars.inflight.push_back((seqno, msg.clone()));
                        conn_vars.rto_duration = Duration::from_secs(1);
                        conn_vars.rto_timer = Some(smol::Timer::new(conn_vars.rto_duration));
                        conn_vars.delayed_ack_timer = None;

                        transmit(msg).await;
                        SteadyState {
                            stream_id,
                            conn_vars,
                        }
                    }
                    Err(_) => {
                        log::trace!("Reset");
                        Reset {
                            stream_id,
                            death: smol::Timer::new(Duration::from_secs(MAX_WAIT_SECS)),
                        }
                    }
                    bad => panic!("impossible state {:?}", bad),
                }
            }
            Reset {
                stream_id,
                mut death,
            } => {
                log::trace!("C={} RESET", stream_id);
                transmit(Message::Rel {
                    kind: RelKind::Rst,
                    stream_id,
                    seqno: 0,
                    ack_seqno: 0,
                    payload: Bytes::new(),
                })
                .await;
                let die = smol::future::race(
                    async {
                        (&mut death).await;
                        true
                    },
                    async {
                        if recv_wire_read.recv().await.is_ok() {
                            true
                        } else {
                            smol::future::pending().await
                        }
                    },
                )
                .await;
                if die {
                    anyhow::bail!("60 seconds in reset up")
                }
                Reset { stream_id, death }
            }
        }
    }
}

pub(crate) struct RelConnBack {
    send_wire_read: Sender<Message>,
}

impl RelConnBack {
    pub fn process(&self, input: Message) {
        drop(self.send_wire_read.try_send(input))
    }
}

pub(crate) struct ConnVars {
    inflight: VecDeque<(Seqno, Message)>,
    next_free_seqno: u64,
    rto_timer: Option<smol::Timer>,
    delayed_ack_timer: Option<smol::Timer>,
    rto_duration: Duration,

    reorderer: Reorderer<Message>,
    lowest_unseen: Seqno,
    // read_buffer: VecDeque<Bytes>,
    cwnd: f64,
    cwnd_max: f64,
    last_loss: Instant,

    dupack_seqno: u64,
    dupack_count: usize,
}

impl Default for ConnVars {
    fn default() -> Self {
        ConnVars {
            inflight: VecDeque::new(),
            next_free_seqno: 0,
            rto_timer: None,
            delayed_ack_timer: None,
            rto_duration: Duration::from_secs(1),

            reorderer: Reorderer::default(),
            lowest_unseen: 0,

            cwnd: 1.0,
            cwnd_max: 100.0,
            last_loss: Instant::now(),
            dupack_seqno: 0,
            dupack_count: 0,
        }
    }
}

impl ConnVars {
    fn congestion_ack(&mut self) {
        self.cubic_update();
        eprintln!("ACK CWND => {}", self.cwnd);
    }

    fn congestion_rto(&mut self) {
        self.cwnd_max = self.cwnd;
        let now = self.cubic_update();
        self.last_loss = now;
        eprintln!("RTO CWND => {}", self.cwnd);
    }

    fn congestion_fast(&mut self) {
        if Instant::now()
            .saturating_duration_since(self.last_loss)
            .as_millis()
            > 500
        {
            self.cwnd_max = self.cwnd;
            let now = self.cubic_update();
            self.last_loss = now;
            eprintln!("FST CWND => {}", self.cwnd);
        }
    }

    fn cubic_update(&mut self) -> Instant {
        let now = Instant::now();
        let t = now.saturating_duration_since(self.last_loss).as_secs_f64();
        let k = (self.cwnd_max / 2.0).powf(0.333);
        let wt = 0.4 * (t - k).powf(3.0) + self.cwnd_max;
        self.cwnd = wt.min(10000.0);
        //self.cwnd = 1500.0;
        now
    }
}
