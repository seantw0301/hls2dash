//! Continuity helpers for stitching live MPEG-TS segments.
//!
//! Origin HLS `.ts` cuts usually restart continuity counters at 0. For a
//! continuous HTTP `/mpegts` stream we rewrite CC so they advance across
//! segments without gaps (same approach as trans_server's continuous layer).

use std::collections::HashMap;

const TS_PACKET_SIZE: usize = 188;

/// Per-PID continuity counter state spanning an entire stitch session.
#[derive(Debug, Default, Clone)]
pub struct ContinuityState {
    /// Last continuity_counter written for each PID that carried a payload.
    last_cc: HashMap<u16, u8>,
}

impl ContinuityState {
    /// Rewrites continuity counters in-place so payload packets form a continuous
    /// sequence per PID across previously applied chunks.
    pub fn rewrite(&mut self, ts: &mut [u8]) {
        assert_eq!(
            ts.len() % TS_PACKET_SIZE,
            0,
            "TS bytes must be whole packets"
        );
        for pkt in ts.chunks_exact_mut(TS_PACKET_SIZE) {
            if pkt[0] != 0x47 {
                continue;
            }
            let pid = (((pkt[1] & 0x1f) as u16) << 8) | pkt[2] as u16;
            let afc = (pkt[3] >> 4) & 0x03;
            let has_payload = (afc & 0x01) != 0;
            if has_payload {
                let next = match self.last_cc.get(&pid) {
                    Some(&prev) => (prev.wrapping_add(1)) & 0x0f,
                    None => 0,
                };
                pkt[3] = (pkt[3] & 0xf0) | next;
                self.last_cc.insert(pid, next);
            } else if let Some(&prev) = self.last_cc.get(&pid) {
                // Adaptation-only: CC must repeat the previous value.
                pkt[3] = (pkt[3] & 0xf0) | (prev & 0x0f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuity_rewrites_across_two_fresh_blobs() {
        let mut a = vec![0u8; 188];
        a[0] = 0x47;
        a[1] = 0x40;
        a[2] = 0x00;
        a[3] = 0x10;

        let mut b = a.clone();
        let mut cc = ContinuityState::default();
        cc.rewrite(&mut a);
        cc.rewrite(&mut b);
        assert_eq!(a[3] & 0x0f, 0);
        assert_eq!(b[3] & 0x0f, 1);
    }
}
