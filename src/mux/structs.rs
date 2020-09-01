use async_channel::Sender;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A sequence number.
pub type Seqno = u64;

/// A message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Message {
    Urel(Bytes),
    Rel {
        kind: RelKind,
        stream_id: u16,
        seqno: Seqno,
        payload: Bytes,
    },
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub enum RelKind {
    Syn,
    SynAck,
    Data,
    DataAck,
    Fin,
    FinAck,
    Rst,
}

#[derive(Clone)]
pub struct Reorderer<T: Clone> {
    pkts: BTreeMap<Seqno, T>,
    min: Seqno,
}

impl<T: Clone> Default for Reorderer<T> {
    fn default() -> Self {
        Reorderer {
            pkts: BTreeMap::new(),
            min: 0,
        }
    }
}

impl<T: Clone> Reorderer<T> {
    pub fn insert(&mut self, seq: Seqno, item: T) -> bool {
        if seq >= self.min && seq <= self.min + 10000 {
            self.pkts.insert(seq, item);
            true
        } else {
            log::trace!("rejecting (seq={}, min={})", seq, self.min);
            false
        }
    }
    pub fn take(&mut self, output: &Sender<T>) -> u64 {
        let mut times = 0;
        for idx in self.min.. {
            if let Some(item) = self.pkts.get(&idx) {
                if output.try_send(item.clone()).is_ok() {
                    self.pkts.remove(&idx);
                } else {
                    break;
                }
                self.min = idx + 1;
                times += 1;
            } else {
                break;
            }
        }
        times
    }
}
