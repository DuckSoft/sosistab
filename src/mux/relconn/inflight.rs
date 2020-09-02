use crate::mux::structs::*;
use rand::prelude::*;
use std::{
    cmp::Reverse,
    collections::VecDeque,
    time::{Duration, Instant},
};

pub struct InflightEntry {
    seqno: Seqno,
    acked: bool,
    send_time: Instant,
    pub retrans: u64,
    pub payload: Message,
}

#[derive(Default)]
pub struct Inflight {
    segments: VecDeque<InflightEntry>,
    times: priority_queue::PriorityQueue<Seqno, Reverse<Instant>>,
    rtt: RttCalculator,
}

impl Inflight {
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn rto(&self) -> Duration {
        self.rtt.rto()
    }

    pub fn srtt(&self) -> u64 {
        self.rtt.srtt
    }

    pub fn bdp(&self) -> f64 {
        self.rtt.bdp()
    }

    pub fn mark_acked(&mut self, seqno: Seqno) {
        // mark the right one
        if let Some(entry) = self.segments.front() {
            let first_seqno = entry.seqno;
            if seqno >= first_seqno {
                let offset = (seqno - first_seqno) as usize;
                if let Some(seg) = self.segments.get_mut(offset) {
                    seg.acked = true;
                    if seg.retrans == 0 {
                        self.rtt
                            .record_sample(Instant::now().saturating_duration_since(seg.send_time));
                    }
                }
                // if offset >= 3 {
                //     // insert synthetic times
                //     let now = Instant::now();
                //     for (seqno, acked, _, _) in self.segments.iter().take(offset) {
                //         if !*acked {
                //             self.times
                //                 .push(*seqno, Reverse(now + Duration::from_millis(10)));
                //         }
                //     }
                // }
                // shrink if possible
                while self.len() > 0 && self.segments.front().unwrap().acked {
                    self.segments.pop_front();
                }
            }
        }
    }

    pub fn insert(&mut self, seqno: Seqno, msg: Message) {
        let rto = self.rtt.rto();
        if self.get_seqno(seqno).is_none() {
            self.segments.push_back(InflightEntry {
                seqno,
                acked: false,
                send_time: Instant::now(),
                payload: msg,
                retrans: 0,
            });
        }
        self.times.push(seqno, Reverse(Instant::now() + rto));
    }

    pub fn get_seqno(&mut self, seqno: Seqno) -> Option<&mut InflightEntry> {
        if let Some(first_entry) = self.segments.front() {
            let first_seqno = first_entry.seqno;
            if seqno >= first_seqno {
                let offset = (seqno - first_seqno) as usize;
                return self.segments.get_mut(offset);
            }
        }
        None
    }

    pub async fn wait_first(&mut self) -> Seqno {
        while !self.times.is_empty() {
            let (_, time) = self.times.peek().unwrap();
            let time = time.0.saturating_duration_since(Instant::now());
            smol::Timer::after(time).await;
            let (seqno, _) = self.times.pop().unwrap();
            let rto = self.rtt.rto();
            if let Some(seg) = self.get_seqno(seqno) {
                seg.retrans += 1;
                // eprintln!(
                //     "retransmitting seqno {} {} times after {}ms",
                //     seg.seqno,
                //     seg.retrans,
                //     Instant::now()
                //         .saturating_duration_since(seg.send_time)
                //         .as_millis()
                // );
                let rtx = seg.retrans;
                let maxrto = rto
                    * rand::thread_rng().gen_range(2u32.pow(rtx as u32 - 1), 2u32.pow(rtx as u32));
                self.times.push(seqno, Reverse(Instant::now() + maxrto));
                return seqno;
            }
        }
        smol::future::pending().await
    }
}

struct RttCalculator {
    srtt: u64,
    rttvar: u64,
    rto: u64,
    existing: bool,

    deliv_rate: f64,
    last_deliv_time: Instant,
    dels_since_last: u64,
}

impl Default for RttCalculator {
    fn default() -> Self {
        RttCalculator {
            srtt: 1000,
            rttvar: 1000,
            rto: 1000,
            deliv_rate: 100.0,
            last_deliv_time: Instant::now(),
            existing: false,
            dels_since_last: 0,
        }
    }
}

impl RttCalculator {
    fn record_sample(&mut self, sample: Duration) {
        let sample = sample.as_millis() as u64;
        if !self.existing {
            self.srtt = sample;
            self.rttvar = sample / 2;
        } else {
            self.rttvar = self.rttvar * 3 / 4 + diff(self.srtt, sample) / 4;
            self.srtt = self.srtt * 7 / 8 + sample / 8;
        }
        self.rto = self.srtt + (4 * self.rttvar).max(10);
        // // delivery rate
        // self.dels_since_last += 1;
        // if self.dels_since_last >= 100 {
        //     let now = Instant::now();
        //     let drate_sample = 100.0 / now.duration_since(self.last_deliv_time).as_secs_f64();
        //     self.deliv_rate = drate_sample * 0.1 + self.deliv_rate * 0.9;
        //     self.last_deliv_time = now;
        //     self.dels_since_last = 0;
        //     dbg!(self.deliv_rate);
        // }
    }

    fn bdp(&self) -> f64 {
        self.deliv_rate * (self.srtt as f64) / 1000.0
    }

    fn rto(&self) -> Duration {
        Duration::from_millis(self.rto)
    }
}

fn diff(a: u64, b: u64) -> u64 {
    if b > a {
        b - a
    } else {
        a - b
    }
}
