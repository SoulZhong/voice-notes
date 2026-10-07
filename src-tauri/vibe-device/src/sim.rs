//! Simulated Device for end-to-end tests without the hardware.
//!
//! Plays the Device role in place of [`crate::ble::run`]: everything behind the
//! link (protocol decoding, Dictation, recognition, insertion, dictation notes,
//! the UI) runs for real. Enabled by the host only when `VN_DEVICE_SIM` names a
//! directory; never in normal use.
//!
//! The directory holds two files:
//! - `cmd`: commands appended one per line, read as they arrive:
//!   `dict <wav> [hold_ms]` (16 kHz mono 16-bit WAV, sent in real time, then
//!   held `hold_ms` before DICT_STOP), `cancel <wav>`, `submit`, `undo`,
//!   `targets <list>`, `select <list> <index>`, `notes`, `alert_open <id>`,
//!   `alert_dismiss <id>`, `hello`, `sleep <ms>`, `disconnect`.
//! - `rx.log`: every frame the Companion sends, decoded, one per line, as
//!   the Device would show it.

use crate::adpcm::{AdpcmState, encode};
use crate::ble::{EventSink, LinkEnd, LinkEvent, LinkState, StateSink};
use crate::protocol::{
    AudioFrame, CompanionFrame, DeviceFrame, NAME_PREFIX, PROTOCOL_VERSION, SAMPLES_PER_FRAME,
};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::sleep;

/// The simulated Device's name.
pub fn device_name() -> String {
    format!("{NAME_PREFIX}SIM0")
}

/// Read a 16 kHz mono 16-bit PCM WAV.
pub fn read_wav(path: &Path) -> Result<Vec<i16>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(format!("{}: not a WAV file", path.display()));
    }
    let mut at = 12;
    let mut format_ok = false;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]]) as usize;
        let body = &bytes[at + 8..(at + 8 + len).min(bytes.len())];
        match id {
            b"fmt " if body.len() >= 16 => {
                let channels = u16::from_le_bytes([body[2], body[3]]);
                let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                let bits = u16::from_le_bytes([body[14], body[15]]);
                if channels != 1 || rate != 16_000 || bits != 16 {
                    return Err(format!(
                        "{}: need 16 kHz mono 16-bit, got {rate} Hz {channels} ch {bits} bit",
                        path.display()
                    ));
                }
                format_ok = true;
            }
            b"data" if format_ok => {
                return Ok(body.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect());
            }
            _ => {}
        }
        at += 8 + len + (len & 1);
    }
    Err(format!("{}: no PCM data", path.display()))
}

/// The AUDIO frames a Device sends for `pcm` (each frame restarts the codec
/// from its own header, as the firmware does).
pub fn audio_frames(dict: u8, pcm: &[i16]) -> Vec<DeviceFrame> {
    pcm.chunks(SAMPLES_PER_FRAME)
        .enumerate()
        .map(|(seq, chunk)| {
            let mut samples = chunk.to_vec();
            samples.resize(SAMPLES_PER_FRAME, 0);
            let pred = samples[0];
            let mut st = AdpcmState::new(pred, 0);
            DeviceFrame::Audio(AudioFrame {
                dict,
                seq: seq as u16,
                pred,
                index: 0,
                adpcm: encode(&mut st, &samples),
            })
        })
        .collect()
}

struct Sim {
    dir: PathBuf,
    events: EventSink,
    started: Instant,
    dict: u8,
}

impl Sim {
    fn send(&self, f: DeviceFrame) {
        if !matches!(f, DeviceFrame::Audio(_)) {
            self.log(&format!("-> {f:?}"));
        }
        (self.events)(LinkEvent::Frame(f.encode()));
    }

    fn log(&self, line: &str) {
        let path = self.dir.join("rx.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{:>8.3} {line}", self.started.elapsed().as_secs_f64());
        }
    }

    async fn speak(&mut self, wav: &str, hold_ms: u64, cancel: bool) {
        let pcm = match read_wav(Path::new(wav)) {
            Ok(p) => p,
            Err(e) => {
                self.log(&format!("!! {e}"));
                return;
            }
        };
        self.dict = self.dict.wrapping_add(1);
        let dict = self.dict;
        self.send(DeviceFrame::DictStart { dict });
        let frames = audio_frames(dict, &pcm);
        self.log(&format!("-> AUDIO x{} ({:.1} s)", frames.len(), pcm.len() as f64 / 16_000.0));
        let pace = Duration::from_millis((SAMPLES_PER_FRAME as u64 * 1000) / 16_000);
        let t0 = Instant::now();
        for (i, f) in frames.into_iter().enumerate() {
            self.send(f);
            let due = pace * (i as u32 + 1);
            if let Some(wait) = due.checked_sub(t0.elapsed()) {
                sleep(wait).await;
            }
        }
        sleep(Duration::from_millis(hold_ms)).await;
        if cancel {
            self.send(DeviceFrame::DictCancel { dict });
        } else {
            self.send(DeviceFrame::DictStop { dict });
        }
    }

    /// Run one command line. False: disconnect.
    async fn run_line(&mut self, line: &str) -> bool {
        let mut words = line.split_whitespace();
        let Some(cmd) = words.next() else { return true };
        let args: Vec<&str> = words.collect();
        let num = |i: usize| args.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        self.log(&format!("## {line}"));
        match cmd {
            "hello" => self.send(DeviceFrame::Hello {
                ver: PROTOCOL_VERSION,
                fw: "VibeVoice/sim".into(),
            }),
            "dict" | "cancel" => {
                let hold = if args.len() > 1 { num(1) } else { 300 };
                self.speak(args.first().copied().unwrap_or(""), hold, cmd == "cancel").await
            }
            "submit" => self.send(DeviceFrame::Submit),
            "undo" => self.send(DeviceFrame::Undo),
            "targets" => self.send(DeviceFrame::TargetsReq { list: num(0) as u8 }),
            "select" => self.send(DeviceFrame::TargetSelect {
                list: num(0) as u8,
                index: num(1) as u8,
            }),
            "notes" => self.send(DeviceFrame::NotesToggle),
            "alert_open" => self.send(DeviceFrame::AlertOpen { id: num(0) as u8 }),
            "alert_dismiss" => self.send(DeviceFrame::AlertDismiss { id: num(0) as u8 }),
            "sleep" => sleep(Duration::from_millis(num(0))).await,
            "disconnect" => return false,
            other => self.log(&format!("!! unknown command {other}")),
        }
        true
    }
}

/// Stand in for [`crate::ble::run`]: link at once, then follow `dir/cmd`.
pub async fn run(dir: PathBuf, mut outgoing: UnboundedReceiver<Vec<u8>>, events: EventSink, state: StateSink) -> LinkEnd {
    let _ = std::fs::create_dir_all(&dir);
    let cmd_path = dir.join("cmd");
    // Commands written before this run are history.
    let mut offset = std::fs::metadata(&cmd_path).map(|m| m.len()).unwrap_or(0);
    let name = device_name();
    let mut sim = Sim {
        dir,
        events: events.clone(),
        started: Instant::now(),
        dict: 0,
    };
    sim.log(&format!("== {name} linked"));
    state(&LinkState::Connected { name: name.clone() });
    events(LinkEvent::Connected(name));
    sim.run_line("hello").await;
    // Replies are logged as they arrive, also while a command (speaking,
    // sleeping) is still running.
    let rx_dir = sim.dir.clone();
    let started = sim.started;
    let replies = tokio::spawn(async move {
        let log = |line: String| {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(rx_dir.join("rx.log")) {
                let _ = writeln!(f, "{:>8.3} {line}", started.elapsed().as_secs_f64());
            }
        };
        while let Some(bytes) = outgoing.recv().await {
            match CompanionFrame::decode(&bytes) {
                Ok(f) => log(format!("<- {f:?}")),
                Err(e) => log(format!("<- !! {e}")),
            }
        }
    });
    let mut poll = tokio::time::interval(Duration::from_millis(200));
    loop {
        poll.tick().await;
        if replies.is_finished() {
            return LinkEnd::Stopped;
        }
        let Ok(text) = std::fs::read(&cmd_path) else { continue };
        if (text.len() as u64) <= offset {
            continue;
        }
        let new = String::from_utf8_lossy(&text[offset as usize..]).into_owned();
        // Only whole lines; a half-written one waits for the next poll.
        let Some(end) = new.rfind('\n') else { continue };
        offset += end as u64 + 1;
        for line in new[..end].lines() {
            if !sim.run_line(line.trim()).await {
                sim.log("== disconnected");
                replies.abort();
                events(LinkEvent::Disconnected);
                state(&LinkState::Retrying);
                return LinkEnd::Stopped;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioAssembler;

    #[test]
    fn audio_frames_round_trip_through_the_assembler() {
        let pcm: Vec<i16> = (0..SAMPLES_PER_FRAME * 3 + 50)
            .map(|n| ((n as f32 * 0.05).sin() * 4000.0) as i16)
            .collect();
        let frames = audio_frames(7, &pcm);
        assert_eq!(frames.len(), 4);
        let mut asm = AudioAssembler::new(7);
        let mut out = Vec::new();
        for f in &frames {
            let DeviceFrame::Audio(a) = f else { panic!() };
            assert!(f.encode().len() <= crate::protocol::MAX_FRAME);
            out.extend(asm.push(a).unwrap());
        }
        let err: f64 = pcm.iter().zip(&out).map(|(a, b)| (*a as f64 - *b as f64).abs()).sum::<f64>() / pcm.len() as f64;
        assert!(err < 300.0, "mean abs error {err}");
    }

    #[test]
    fn reads_a_16k_mono_wav() {
        let samples: Vec<i16> = vec![1, -2, 300, -400];
        let mut b = Vec::new();
        b.extend(b"RIFF");
        b.extend(&(36u32 + 8).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(&16u32.to_le_bytes());
        b.extend(&1u16.to_le_bytes());
        b.extend(&1u16.to_le_bytes());
        b.extend(&16_000u32.to_le_bytes());
        b.extend(&32_000u32.to_le_bytes());
        b.extend(&2u16.to_le_bytes());
        b.extend(&16u16.to_le_bytes());
        b.extend(b"data");
        b.extend(&8u32.to_le_bytes());
        for s in &samples {
            b.extend(&s.to_le_bytes());
        }
        let dir = std::env::temp_dir().join(format!("vd-sim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.wav");
        std::fs::write(&p, b).unwrap();
        assert_eq!(read_wav(&p).unwrap(), samples);
        let _ = std::fs::remove_dir_all(dir);
    }
}
