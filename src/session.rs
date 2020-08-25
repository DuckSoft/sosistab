use crate::fec::{FrameDecoder, FrameEncoder};
use crate::msg::DataFrame;
use crate::runtime;
use async_channel::{Receiver, Sender};
use bytes::Bytes;
use smol::prelude::*;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant};

async fn infal<T, E, F: Future<Output = std::result::Result<T, E>>>(fut: F) -> T {
    match fut.await {
        Ok(res) => res,
        Err(_) => {
            smol::future::pending::<()>().await;
            unreachable!();
        }
    }
}

pub struct SessionConfig {
    pub latency: Duration,
    pub target_loss: f64,
    pub send_frame: Sender<DataFrame>,
    pub recv_frame: Receiver<DataFrame>,
}

/// Representation of an isolated session that deals only in DataFrames and abstracts away all I/O concerns. It's the user's responsibility to poll the session. Otherwise, it might not make progress and will drop packets.
pub struct Session {
    send_tosend: Sender<Bytes>,
    recv_input: Receiver<Bytes>,
    _dropper: Vec<Box<dyn FnOnce() + Send + Sync + 'static>>,
    _task: smol::Task<()>,
}

impl Session {
    /// Creates a tuple of a Session and also a channel with which stuff is fed into the session.
    pub fn new(cfg: SessionConfig) -> Self {
        let (s2, r2) = async_channel::bounded(100);
        let (s4, r4) = async_channel::bounded(100);
        let task = runtime::spawn(session_loop(cfg, r2, s4));
        Session {
            send_tosend: s2,
            recv_input: r4,
            _dropper: Vec::new(),
            _task: task,
        }
    }

    /// Adds a closure to be run when the Session is dropped. Use this to manage associated "worker" resources.
    pub fn on_drop<T: FnOnce() + Send + Sync + 'static>(&mut self, thing: T) {
        self._dropper.push(Box::new(thing))
    }

    /// Takes a Bytes to be sent and stuffs it into the session.
    pub async fn send_bytes(&self, to_send: Bytes) {
        drop(self.send_tosend.try_send(to_send));
        smol::future::yield_now().await;
    }

    /// Waits until the next application input is decoded by the session.
    pub async fn recv_bytes(&self) -> Bytes {
        self.recv_input.recv().await.unwrap()
    }
}

async fn session_loop(cfg: SessionConfig, recv_tosend: Receiver<Bytes>, send_input: Sender<Bytes>) {
    let measured_loss = AtomicU8::new(0);
    let high_recv_frame_no = AtomicU64::new(0);
    let total_recv_frames = AtomicU64::new(0);
    // sending loop
    let send_loop = async {
        let mut frame_no = 0u64;
        let mut run_no = 0u64;
        let mut to_send = Vec::new();
        loop {
            // obtain a vector of bytes to send
            let to_send = {
                to_send.clear();
                // get as much tosend as possible within the timeout
                // this lets us do raptorq at maximum efficiency
                to_send.push(infal(recv_tosend.recv()).await);
                let mut timeout = smol::Timer::new(cfg.latency);
                loop {
                    let res = smol::future::race(
                        async {
                            (&mut timeout).await;
                            true
                        },
                        async {
                            to_send.push(infal(recv_tosend.recv()).await);
                            false
                        },
                    );
                    if res.await || to_send.len() >= 10 {
                        break &to_send;
                    }
                }
            };
            let run_len: u32 = to_send.iter().map(|b| b.len() + 2).sum::<usize>() as u32;
            // encode into raptor
            let encoded = FrameEncoder::new(loss_to_u8(cfg.target_loss))
                .encode(measured_loss.load(Ordering::Relaxed), &to_send);
            // log::trace!(
            //     "send_loop: encoding {} => {} frames",
            //     to_send.len(),
            //     encoded.len()
            // );
            for bts in encoded {
                drop(
                    cfg.send_frame
                        .send(DataFrame {
                            frame_no,
                            run_no,
                            run_len,
                            body: bts,
                            high_recv_frame_no: high_recv_frame_no.load(Ordering::Relaxed),
                            total_recv_frames: total_recv_frames.load(Ordering::Relaxed),
                        })
                        .await,
                );
                frame_no += 1;
            }
            run_no += 1;
        }
    };
    // receive loop
    let recv_loop = async {
        let mut rp_filter = ReplayFilter::new(0);
        let mut reconstruction_buffer = None;
        let mut run_no = 0u64;
        let mut loss_calc = LossCalculator::new();
        loop {
            let new_frame = infal(cfg.recv_frame.recv()).await;
            if !rp_filter.add(new_frame.frame_no) {
                log::trace!(
                    "recv_loop: replay filter dropping frame {}",
                    new_frame.frame_no
                );
                continue;
            }
            loss_calc.update_params(new_frame.high_recv_frame_no, new_frame.total_recv_frames);
            measured_loss.store(loss_to_u8(loss_calc.median), Ordering::Relaxed);
            high_recv_frame_no.fetch_max(new_frame.frame_no, Ordering::Relaxed);
            total_recv_frames.fetch_add(1, Ordering::Relaxed);
            if new_frame.run_no > run_no || reconstruction_buffer.is_none() {
                reconstruction_buffer =
                    Some(FrameDecoder::new(new_frame.run_no, new_frame.run_len));
                run_no = new_frame.run_no;
            }
            let rcb = reconstruction_buffer.as_mut().unwrap();
            if let Some(output) = rcb.decode(&new_frame.body) {
                for item in output {
                    let _ = send_input.try_send(item);
                }
            }
        }
    };
    smol::future::race(send_loop, recv_loop).await;
}

/// A filter for replays. Records recently seen seqnos and rejects either repeats or really old seqnos.
#[derive(Debug)]
struct ReplayFilter {
    top_seqno: u64,
    bottom_seqno: u64,
    seen_seqno: HashSet<u64>,
}

impl ReplayFilter {
    fn new(start: u64) -> Self {
        ReplayFilter {
            top_seqno: start,
            bottom_seqno: start,
            seen_seqno: HashSet::new(),
        }
    }

    fn add(&mut self, seqno: u64) -> bool {
        if seqno < self.bottom_seqno {
            // out of range. we can't know, so we just say no
            return false;
        }
        // check the seen
        if self.seen_seqno.contains(&seqno) {
            return false;
        }
        self.top_seqno = seqno;
        while self.top_seqno - self.bottom_seqno > 1000 {
            self.seen_seqno.remove(&self.bottom_seqno);
            self.bottom_seqno += 1;
        }
        true
    }
}

fn loss_to_u8(loss: f64) -> u8 {
    let loss = loss * 256.0;
    if loss > 254.0 {
        return 255;
    }
    loss as u8
}

/// A packet loss calculator.
struct LossCalculator {
    last_top_seqno: u64,
    last_total_seqno: u64,
    last_time: Instant,
    loss_samples: VecDeque<f64>,
    median: f64,
}

impl LossCalculator {
    fn new() -> LossCalculator {
        LossCalculator {
            last_top_seqno: 0,
            last_total_seqno: 0,
            last_time: Instant::now(),
            loss_samples: VecDeque::new(),
            median: 0.0,
        }
    }

    fn update_params(&mut self, top_seqno: u64, total_seqno: u64) {
        if total_seqno > self.last_total_seqno + 10 && top_seqno > self.last_top_seqno + 10 {
            let delta_top = top_seqno.saturating_sub(self.last_top_seqno) as f64;
            let delta_total = total_seqno.saturating_sub(self.last_total_seqno) as f64;
            self.last_top_seqno = top_seqno;
            self.last_total_seqno = total_seqno;
            let loss_sample = 1.0 - delta_total / delta_top.max(delta_total);
            self.loss_samples.push_back(loss_sample);
            log::trace!("recording loss sample {}", loss_sample);
            if self.loss_samples.len() > 16 {
                self.loss_samples.pop_front();
            }
            let median = {
                let mut lala: Vec<f64> = self.loss_samples.iter().cloned().collect();
                lala.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
                lala[lala.len() / 2]
            };
            self.median = median
        }
    }
}

// #[cfg(test)]
// mod tests {
//     use super::*;
//     use rand::prelude::*;
//     use std::sync::Arc;
//     #[test]
//     fn session_trivial() {
//         drop(env_logger::try_init());
//         smol::run(async {
//             let (send, recv) = async_channel::unbounded();
//             let sess_config = SessionConfig {
//                 latency: Duration::from_millis(10),
//                 target_loss: 0.05,
//                 output: Box::new(move |df| drop(send.try_send(df))),
//             };
//             let session = Session::new(sess_config);
//             let f1 = async {
//                 for _ in 0..1000 {
//                     let mut bts = vec![0u8; 1280];
//                     rand::thread_rng().fill_bytes(&mut bts);
//                     let bts = Bytes::from(bts);
//                     session.send_bytes(bts);
//                     smol::Timer::new(Duration::from_millis(2)).await;
//                 }
//             };
//             let f2 = async {
//                 let mut ctr = 0;
//                 loop {
//                     let next_output = recv.recv().await.unwrap();
//                     if rand::thread_rng().gen::<f64>() < 0.5 {
//                         continue;
//                     }
//                     session.process_input(next_output);
//                     loop {
//                         if let Ok(la) = session.recv_input.try_recv() {
//                             ctr += 1;
//                             dbg!(ctr);
//                         } else {
//                             break;
//                         }
//                     }
//                 }
//             };
//             smol::future::race(f1, f2).await;
//         });
//     }
// }
