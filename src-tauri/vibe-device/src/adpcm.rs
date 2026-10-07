//! IMA-ADPCM (4 bits/sample, low nibble first, standard 89-entry step table).
//!
//! The decoder is what the Companion uses on AUDIO frames; the encoder mirrors
//! the firmware and exists for tests and the simulator.

const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

const INDEX_TABLE: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

/// Encoder/decoder state: predictor and step index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AdpcmState {
    pub predictor: i16,
    pub index: u8,
}

impl AdpcmState {
    pub fn new(predictor: i16, index: u8) -> Self {
        // A corrupt index from the air must not panic the decoder.
        Self {
            predictor,
            index: index.min(88),
        }
    }

    fn apply(&mut self, nibble: u8) -> i16 {
        let step = STEP_TABLE[self.index as usize];
        let mut diff = step >> 3;
        if nibble & 4 != 0 {
            diff += step;
        }
        if nibble & 2 != 0 {
            diff += step >> 1;
        }
        if nibble & 1 != 0 {
            diff += step >> 2;
        }
        let mut pred = self.predictor as i32;
        if nibble & 8 != 0 {
            pred -= diff;
        } else {
            pred += diff;
        }
        self.predictor = pred.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        self.index = (self.index as i32 + INDEX_TABLE[(nibble & 0x0f) as usize]).clamp(0, 88) as u8;
        self.predictor
    }

    /// Decode one nibble.
    pub fn decode_nibble(&mut self, nibble: u8) -> i16 {
        self.apply(nibble & 0x0f)
    }

    /// Encode one sample, returning its nibble.
    pub fn encode_sample(&mut self, sample: i16) -> u8 {
        let step = STEP_TABLE[self.index as usize];
        let mut diff = sample as i32 - self.predictor as i32;
        let mut nibble = 0u8;
        if diff < 0 {
            nibble = 8;
            diff = -diff;
        }
        let mut s = step;
        if diff >= s {
            nibble |= 4;
            diff -= s;
        }
        s >>= 1;
        if diff >= s {
            nibble |= 2;
            diff -= s;
        }
        s >>= 1;
        if diff >= s {
            nibble |= 1;
        }
        // Track exactly what the decoder will reconstruct.
        self.apply(nibble);
        nibble
    }
}

/// Decode `data` (low nibble first) into `out`, starting from `state`.
pub fn decode(state: &mut AdpcmState, data: &[u8], out: &mut Vec<i16>) {
    out.reserve(data.len() * 2);
    for &b in data {
        out.push(state.decode_nibble(b & 0x0f));
        out.push(state.decode_nibble(b >> 4));
    }
}

/// Encode `samples` (even count expected; an odd tail is padded with a zero
/// nibble) starting from `state`.
pub fn encode(state: &mut AdpcmState, samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len().div_ceil(2));
    for pair in samples.chunks(2) {
        let lo = state.encode_sample(pair[0]);
        let hi = if pair.len() > 1 {
            state.encode_sample(pair[1])
        } else {
            0
        };
        out.push(lo | (hi << 4));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Golden {
        input: Vec<i16>,
        encoded: Vec<u8>,
        decoded: Vec<i16>,
        final_predictor: i16,
        final_index: u8,
    }

    fn load_golden() -> Golden {
        // A copy of vibe-voice-input `tests/vectors/adpcm_golden.txt`, the
        // vector the firmware tests too; refresh it when the firmware's changes.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/adpcm_golden.txt");
        let text = std::fs::read_to_string(path).expect("golden vector shared with firmware");
        let mut g = Golden {
            input: vec![],
            encoded: vec![],
            decoded: vec![],
            final_predictor: 0,
            final_index: 0,
        };
        for line in text.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let (key, rest) = line.split_once(' ').unwrap();
            let nums = || rest.split_whitespace().map(|n| n.parse::<i32>().unwrap());
            match key {
                "input" => g.input = nums().map(|n| n as i16).collect(),
                "decoded" => g.decoded = nums().map(|n| n as i16).collect(),
                "encoded" => {
                    let hex = rest.trim();
                    g.encoded = (0..hex.len())
                        .step_by(2)
                        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                        .collect();
                }
                "final_predictor" => g.final_predictor = rest.trim().parse().unwrap(),
                "final_index" => g.final_index = rest.trim().parse().unwrap(),
                other => panic!("unknown key {other}"),
            }
        }
        g
    }

    #[test]
    fn golden_vector_encodes_identically() {
        let g = load_golden();
        assert_eq!(g.input.len(), 320);
        let mut st = AdpcmState::default();
        let enc = encode(&mut st, &g.input);
        assert_eq!(enc, g.encoded);
        assert_eq!(st.predictor, g.final_predictor);
        assert_eq!(st.index, g.final_index);
    }

    #[test]
    fn golden_vector_decodes_identically() {
        let g = load_golden();
        let mut st = AdpcmState::default();
        let mut out = Vec::new();
        decode(&mut st, &g.encoded, &mut out);
        assert_eq!(out, g.decoded);
        assert_eq!(st, AdpcmState::new(g.final_predictor, g.final_index));
    }

    #[test]
    fn corrupt_index_is_clamped() {
        let mut st = AdpcmState::new(0, 200);
        let mut out = Vec::new();
        decode(&mut st, &[0x77, 0x88], &mut out);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn extremes_saturate() {
        let mut st = AdpcmState::new(i16::MAX, 88);
        let mut out = Vec::new();
        decode(&mut st, &[0x77], &mut out);
        assert_eq!(out, vec![i16::MAX, i16::MAX]);
    }
}
