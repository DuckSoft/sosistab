use crate::fec::{FrameDecoder, FrameEncoder};
use crate::msg::DataFrame;
use async_channel::{Receiver, Sender};
use async_mutex::Mutex;
use bytes::Bytes;
use smol::prelude::*;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::time::Duration;

pub struct SessionConfig {
    pub latency: Duration,
    pub target_loss: f64,
}

/// Representation of an isolated session that deals only in DataFrames and abstracts away all I/O concerns. It's the user's responsibility to poll both next_output and process_input.
pub struct Session {
    send_input: Sender<DataFrame>,
    send_tosend: Sender<Bytes>,
    recv_tosend: Mutex<Receiver<DataFrame>>,
    recv_input: Mutex<Receiver<Bytes>>,
    _task: Option<smol::Task<()>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let to_drop = std::mem::replace(&mut self._task, None).unwrap();
        smol::block_on(to_drop.cancel());
    }
}

impl Session {
    pub fn new(cfg: SessionConfig) -> Self {
        // 140 KB. Limits bufferbloat
        let (s1, r1) = async_channel::bounded(100);
        let (s2, r2) = async_channel::bounded(100);
        let (s3, r3) = async_channel::bounded(100);
        let (s4, r4) = async_channel::bounded(100);
        let task = smol::Task::spawn(session_loop(cfg, r1, r2, s3, s4));
        Session {
            send_input: s1,
            send_tosend: s2,
            recv_tosend: Mutex::new(r3),
            recv_input: Mutex::new(r4),
            _task: Some(task),
        }
    }

    pub fn process_input(&self, input: DataFrame) {
        drop(self.send_input.try_send(input));
    }

    pub fn send_bytes(&self, to_send: Bytes) {
        drop(self.send_tosend.try_send(to_send));
    }

    pub async fn next_output(&self) -> DataFrame {
        self.recv_tosend.lock().await.next().await.unwrap()
    }

    pub async fn next_input(&self) -> Bytes {
        self.recv_input.lock().await.next().await.unwrap()
    }
}

async fn session_loop(
    cfg: SessionConfig,
    recv_input: Receiver<DataFrame>,
    recv_tosend: Receiver<Bytes>,
    send_tosend: Sender<DataFrame>,
    send_input: Sender<Bytes>,
) {
    let measured_loss = AtomicU8::new(0);
    let high_recv_frame_no = AtomicU64::new(0);
    let total_recv_frames = AtomicU64::new(0);
    // sending loop
    let send_loop = async {
        let mut frame_no = 0u64;
        let mut run_no = 0u64;
        loop {
            // obtain a vector of bytes to send
            let to_send = {
                let mut vec = Vec::with_capacity(100);
                // get as much tosend as possible within the timeout
                // this lets us do raptorq at maximum efficiency
                vec.push(recv_tosend.recv().await.expect("input channel closed?!"));
                let mut timeout = smol::Timer::new(cfg.latency);
                loop {
                    let res = smol::future::race(
                        async {
                            (&mut timeout).await;
                            true
                        },
                        async {
                            vec.push(recv_tosend.recv().await.unwrap());
                            false
                        },
                    );
                    if res.await || vec.len() >= 100 {
                        break vec;
                    }
                }
            };
            let run_len: u32 = to_send.iter().map(|b| b.len() + 2).sum::<usize>() as u32;
            // encode into raptor
            let encoded = FrameEncoder::new(loss_to_u8(cfg.target_loss))
                .encode(measured_loss.load(Ordering::Relaxed), &to_send);
            log::trace!(
                "send_loop: encoding {} => {} frames",
                to_send.len(),
                encoded.len()
            );
            for bts in encoded {
                let _ = send_tosend.try_send(DataFrame {
                    frame_no,
                    run_no,
                    run_len,
                    body: bts,
                    high_recv_frame_no: high_recv_frame_no.load(Ordering::Relaxed),
                    total_recv_frames: total_recv_frames.load(Ordering::Relaxed),
                });
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
        loop {
            let new_frame = recv_input.recv().await.unwrap();
            if !rp_filter.add(new_frame.frame_no) {
                log::trace!(
                    "recv_loop: replay filter dropping frame {}",
                    new_frame.frame_no
                );
                continue;
            }
            high_recv_frame_no.fetch_max(new_frame.frame_no, Ordering::Relaxed);
            total_recv_frames.fetch_add(1, Ordering::Relaxed);
            if new_frame.run_no > run_no || reconstruction_buffer.is_none() {
                reconstruction_buffer =
                    Some(FrameDecoder::new(new_frame.run_no, new_frame.run_len));
                log::trace!("recv_loop: init frame decoder for {}", run_no);
                run_no = new_frame.run_no;
            }
            let rcb = reconstruction_buffer.as_mut().unwrap();
            if let Some(output) = rcb.decode(&new_frame.body) {
                log::trace!("recv_loop: reconstructed {} payloads", output.len());
                for item in output {
                    let _ = send_input.try_send(item);
                }
            }
        }
    };
    smol::future::race(send_loop, recv_loop).await;
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use rand::prelude::*;
    use std::sync::Arc;
    #[test]
    fn session_trivial() {
        drop(env_logger::try_init());
        smol::run(async {
            let sess_output = {
                let sess_config = SessionConfig {
                    latency: Duration::from_millis(10),
                    target_loss: 0.01,
                };
                let session = Arc::new(Session::new(sess_config));
                {
                    let session = session.clone();
                    smol::Task::spawn(async move {
                        loop {
                            let mut bts = vec![0u8; 1280];
                            rand::thread_rng().fill_bytes(&mut bts);
                            let bts = Bytes::from(bts);
                            session.send_bytes(bts);
                            smol::Timer::new(Duration::from_millis(1)).await;
                        }
                    })
                    .detach();
                }
                loop {
                    let next_output = session.next_output().await;
                    if rand::thread_rng().gen::<f64>() < 0.1 {
                        continue;
                    }
                    session.process_input(next_output);
                }
            };
        });
    }
}
