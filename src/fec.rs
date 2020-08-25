use bytes::Bytes;
use probability::distribution::Distribution;
use raptorq::{Decoder, Encoder, EncodingPacket, ObjectTransmissionInformation};
use std::collections::BTreeMap;
/// A forward error correction encoder. Retains internal state for memoization, memory pooling etc.
#[derive(Debug)]
pub struct FrameEncoder {
    // table mapping current loss in pct + run length => overhead
    rate_table: BTreeMap<(u8, usize), usize>,
    // target loss rate
    target_loss: u8,
    // scratch space for encoding
    scratch_space: Vec<u8>,
    // snap encoder
    snap_encoder: snap::raw::Encoder,
}

impl FrameEncoder {
    /// Creates a new Encoder at the given loss level.
    pub fn new(target_loss: u8) -> Self {
        FrameEncoder {
            rate_table: BTreeMap::new(),
            target_loss,
            scratch_space: Vec::new(),
            snap_encoder: snap::raw::Encoder::new(),
        }
    }

    /// Encodes a slice of packets into more packets.
    pub fn encode(&mut self, measured_loss: u8, pkts: &[Bytes]) -> Vec<Bytes> {
        // first we join the packets into the scratch space
        self.scratch_space.clear();
        pre_encode(pkts, &mut self.scratch_space);
        log::trace!(
            "encoding {} pkts (preencoded {}B) with measured loss {}",
            pkts.len(),
            self.scratch_space.len(),
            measured_loss
        );
        // then we encode it
        let rq_encoder = Encoder::with_defaults(&self.scratch_space, 1300);
        let mut out = Vec::new();
        for blk_enc in rq_encoder.get_block_encoders() {
            let srcpkts = blk_enc.source_packets();
            let outpkt_count = self.repair_len(measured_loss, srcpkts.len());
            let encoder = &mut self.snap_encoder;
            out.extend(blk_enc.source_packets().iter().map(|b| {
                let uncompressed = b.serialize();
                Bytes::from(encoder.compress_vec(&uncompressed).unwrap())
            }));
            out.extend(
                blk_enc
                    .repair_packets(0, outpkt_count as u32)
                    .iter()
                    .map(|b| {
                        let uncompressed = b.serialize();
                        Bytes::from(encoder.compress_vec(&uncompressed).unwrap())
                    }),
            );
        }
        out
    }

    /// Calculates the number of repair blocks needed to properly reconstruct a run of packets.
    fn repair_len(&mut self, measured_loss: u8, run_len: usize) -> usize {
        let target_loss = self.target_loss;
        *self
            .rate_table
            .entry((measured_loss, run_len))
            .or_insert_with(|| {
                for additional_len in 0.. {
                    let distro = probability::distribution::Binomial::with_failure(
                        run_len + additional_len,
                        (measured_loss as f64 / 256.0).max(1e-100).min(1.0 - 1e-100),
                    );
                    let result_loss = distro.distribution(run_len as f64);
                    if result_loss <= target_loss as f64 / 256.0 {
                        return additional_len.saturating_sub(1usize);
                    }
                }
                panic!()
            })
    }
}

/// A single-use FEC decoder.
#[derive(Debug)]
pub struct FrameDecoder {
    run_no: u64,
    rq_decoder: Decoder,
    done: bool,
}

impl FrameDecoder {
    pub fn new(run_no: u64, total_len: u32) -> Self {
        FrameDecoder {
            run_no,
            rq_decoder: Decoder::new(ObjectTransmissionInformation::with_defaults(
                total_len as u64,
                1300,
            )),
            done: false,
        }
    }

    pub fn decode(&mut self, pkt: &[u8]) -> Option<Vec<Bytes>> {
        let pkt = snap::raw::Decoder::new().decompress_vec(pkt).ok()?;
        if pkt.len() < 4 || self.done {
            return None;
        }
        let raw = Bytes::from(self.rq_decoder.decode(EncodingPacket::deserialize(&pkt))?);
        let out = post_decode(raw)?;
        self.done = true;
        Some(out)
    }
}

fn pre_encode(pkts: &[Bytes], scratch: &mut Vec<u8>) {
    for p in pkts {
        scratch.extend_from_slice(&(p.len() as u16).to_le_bytes());
        scratch.extend_from_slice(&p);
    }
}

fn post_decode(raw: Bytes) -> Option<Vec<Bytes>> {
    let mut out = Vec::with_capacity(raw.len() / 1000);
    if raw.len() < 2 {
        return None;
    }
    let mut ptr = raw.clone();
    while !ptr.is_empty() {
        let pkt_len = u16::from_le_bytes([ptr[0], ptr[1]]) as usize;
        if raw.len() < pkt_len + 2 {
            return None;
        }
        out.push(ptr.slice(2..2 + (pkt_len as usize)));
        ptr = ptr.slice(2 + (pkt_len as usize)..)
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    extern crate test;
    use super::*;

    #[bench]
    fn bench_frame_encoder(b: &mut test::Bencher) {
        let lala = vec![Bytes::from([0u8; 1024].as_ref()); 10];
        let mut encoder = FrameEncoder::new(1);
        b.iter(|| {
            encoder.encode(0, &lala);
        })
    }
}
