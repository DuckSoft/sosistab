use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, num::Wrapping};

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
        ack_seqno: Seqno,
        payload: Bytes,
    },
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub enum RelKind {
    Syn,
    SynAck,
    Data,
    Fin,
    FinAck,
    Rst,
}

#[derive(Clone)]
pub struct Reorderer<T> {
    pkts: BTreeMap<Seqno, T>,
    min: Seqno,
}

impl<T> Default for Reorderer<T> {
    fn default() -> Self {
        Reorderer {
            pkts: BTreeMap::new(),
            min: 0,
        }
    }
}

impl<T> Reorderer<T> {
    pub fn insert(&mut self, seq: Seqno, item: T) {
        if seq >= self.min && seq <= self.min + 1000 {
            self.pkts.insert(seq, item);
        }
    }
    pub fn take(&mut self) -> Vec<T> {
        let mut output = Vec::with_capacity(self.pkts.len());
        for idx in self.min.. {
            if let Some(item) = self.pkts.remove(&idx) {
                output.push(item);
                self.min = idx + 1;
            } else {
                break;
            }
        }
        output
    }
}
