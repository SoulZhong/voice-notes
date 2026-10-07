//! Turns AUDIO frames of one Dictation into a continuous 16 kHz PCM stream.
//!
//! Each frame decodes on its own (it carries the encoder state). Lost frames
//! (gaps in `seq`) are replaced by silence so timing stays right; late or
//! duplicate frames are dropped.

use crate::adpcm::{self, AdpcmState};
use crate::protocol::{AudioFrame, SAMPLES_PER_FRAME};

/// Longest gap filled with silence (1 s). Longer gaps are clamped.
pub const MAX_GAP_FRAMES: u16 = 50;
pub const SAMPLE_RATE: u32 = 16_000;

#[derive(Debug)]
pub struct AudioAssembler {
    dict: u8,
    next_seq: Option<u16>,
    pub frames: u64,
    pub lost_frames: u64,
    pub dropped_frames: u64,
}

impl AudioAssembler {
    pub fn new(dict: u8) -> Self {
        Self {
            dict,
            next_seq: None,
            frames: 0,
            lost_frames: 0,
            dropped_frames: 0,
        }
    }

    /// Decode `frame`, returning the PCM to feed (silence for any gap first).
    /// Returns `None` for frames that belong elsewhere or arrive late.
    pub fn push(&mut self, frame: &AudioFrame) -> Option<Vec<i16>> {
        if frame.dict != self.dict {
            self.dropped_frames += 1;
            return None;
        }
        let mut pcm = Vec::with_capacity(SAMPLES_PER_FRAME);
        if let Some(expected) = self.next_seq {
            let delta = frame.seq.wrapping_sub(expected);
            if delta >= 0x8000 {
                // Older than what we already played: duplicate or reordered.
                self.dropped_frames += 1;
                return None;
            }
            if delta > 0 {
                self.lost_frames += delta as u64;
                let fill = delta.min(MAX_GAP_FRAMES) as usize * SAMPLES_PER_FRAME;
                pcm.resize(fill, 0);
            }
        }
        self.next_seq = Some(frame.seq.wrapping_add(1));
        self.frames += 1;
        let mut st = AdpcmState::new(frame.pred, frame.index);
        adpcm::decode(&mut st, &frame.adpcm, &mut pcm);
        Some(pcm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(dict: u8, seq: u16) -> AudioFrame {
        AudioFrame {
            dict,
            seq,
            pred: 100,
            index: 10,
            adpcm: vec![0x11; 160],
        }
    }

    #[test]
    fn contiguous_frames_yield_320_samples() {
        let mut a = AudioAssembler::new(1);
        assert_eq!(a.push(&frame(1, 0)).unwrap().len(), 320);
        assert_eq!(a.push(&frame(1, 1)).unwrap().len(), 320);
        assert_eq!(a.lost_frames, 0);
    }

    #[test]
    fn gap_inserts_silence() {
        let mut a = AudioAssembler::new(1);
        a.push(&frame(1, 0)).unwrap();
        let pcm = a.push(&frame(1, 3)).unwrap();
        assert_eq!(pcm.len(), 3 * 320);
        assert!(pcm[..640].iter().all(|&s| s == 0));
        assert_eq!(a.lost_frames, 2);
    }

    #[test]
    fn huge_gap_is_clamped() {
        let mut a = AudioAssembler::new(1);
        a.push(&frame(1, 0)).unwrap();
        let pcm = a.push(&frame(1, 1000)).unwrap();
        assert_eq!(pcm.len(), (MAX_GAP_FRAMES as usize + 1) * 320);
    }

    #[test]
    fn seq_wraps_and_duplicates_drop() {
        let mut a = AudioAssembler::new(2);
        a.push(&frame(2, 65535)).unwrap();
        assert_eq!(a.push(&frame(2, 0)).unwrap().len(), 320);
        assert!(a.push(&frame(2, 0)).is_none());
        assert!(a.push(&frame(2, 65535)).is_none());
        assert_eq!(a.dropped_frames, 2);
    }

    #[test]
    fn other_dictation_frames_ignored() {
        let mut a = AudioAssembler::new(2);
        assert!(a.push(&frame(3, 0)).is_none());
    }

    #[test]
    fn frame_state_is_independent() {
        // Two frames with identical payload and header decode identically
        // regardless of what came before.
        let mut a = AudioAssembler::new(0);
        let x = a.push(&frame(0, 0)).unwrap();
        let y = a.push(&frame(0, 1)).unwrap();
        assert_eq!(x, y);
    }
}
