//! Vibe Voice BLE protocol v2 codec (see `docs/vibe-voice/protocol.md`).
//!
//! Pure data handling: no I/O. Every encoded frame is at most [`MAX_FRAME`]
//! bytes; text that does not fit is sent as its tail, cut on a UTF-8 boundary.

use std::fmt;

/// Protocol version carried in HELLO / HELLO_ACK.
pub const PROTOCOL_VERSION: u8 = 2;
/// Maximum size of one frame (one GATT write or notification).
pub const MAX_FRAME: usize = 180;
/// Samples carried by one AUDIO frame (20 ms at 16 kHz).
pub const SAMPLES_PER_FRAME: usize = 320;
/// ADPCM payload bytes in one AUDIO frame.
pub const ADPCM_BYTES: usize = SAMPLES_PER_FRAME / 2;

pub const NUS_SERVICE: &str = "6e400001-b5a3-f393-e0a9-e50e24dcca9e";
pub const NUS_RX: &str = "6e400002-b5a3-f393-e0a9-e50e24dcca9e";
pub const NUS_TX: &str = "6e400003-b5a3-f393-e0a9-e50e24dcca9e";
/// Advertised name prefix of a Device.
pub const NAME_PREFIX: &str = "VibeVoice-";

pub mod ty {
    pub const HELLO: u8 = 0x01;
    pub const DICT_START: u8 = 0x10;
    pub const AUDIO: u8 = 0x11;
    pub const DICT_STOP: u8 = 0x12;
    pub const DICT_CANCEL: u8 = 0x13;
    pub const SUBMIT: u8 = 0x20;
    pub const UNDO: u8 = 0x21;
    pub const TARGETS_REQ: u8 = 0x30;
    pub const TARGET_SELECT: u8 = 0x31;
    pub const NOTES_TOGGLE: u8 = 0x40;
    pub const ALERT_OPEN: u8 = 0x50;
    pub const ALERT_DISMISS: u8 = 0x51;

    pub const HELLO_ACK: u8 = 0x81;
    pub const STATUS: u8 = 0x82;
    pub const PARTIAL: u8 = 0x90;
    pub const RESULT: u8 = 0x91;
    pub const ACTION_RESULT: u8 = 0xA0;
    pub const TARGET_ITEM: u8 = 0xB0;
    pub const TARGET_END: u8 = 0xB1;
    pub const TARGET_STATE: u8 = 0xB2;
    pub const NOTES_STATE: u8 = 0xC0;
    pub const ALERT: u8 = 0xD0;
    pub const ALERT_CLEAR: u8 = 0xD1;
    pub const ALERT_MORE: u8 = 0xD2;
}

/// RESULT / ACTION_RESULT / TARGET_STATE status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Ok = 0,
    Empty = 1,
    Cancelled = 2,
    TargetUnavailable = 3,
    RecognizerError = 4,
    Permission = 5,
    NothingToUndo = 6,
}

/// STATUS frame codes (Companion-level problems).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StatusCode {
    Clear = 0,
    SpeechPermission = 1,
    AccessibilityPermission = 2,
    OrcaUnavailable = 3,
    RecognizerUnavailable = 4,
}

/// TARGET_STATE kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TargetKind {
    /// Reserved (v1 "follow focus"); never sent.
    Unknown = 0,
    /// The Current Conversation of a Supported App other than Orca.
    App = 1,
    /// Orca's Current Conversation (an Orca Session).
    Orca = 2,
}

/// Which list a TARGETS_REQ / TARGET_ITEM refers to: `0` is the root list of
/// Supported Apps, `n` (1..=4) the conversations of root row `n - 1`.
pub const LIST_ROOT: u8 = 0;
pub const LIST_ORCA: u8 = 1;

/// TARGET_STATE `app` when the Target is in no Supported App.
pub const APP_NONE: u8 = 0xFF;

/// TARGET_ITEM flags.
/// Root list: the app the Target is in. Sub-list: the Current Conversation.
pub const FLAG_CURRENT: u8 = 0x01;
/// The row opens a sub-list (every root row).
pub const FLAG_SUBLIST: u8 = 0x02;
/// The app is not running.
pub const FLAG_NOT_RUNNING: u8 = 0x04;

/// NOTES_STATE `state`: the Voice Notes Recording as the Device shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NotesState {
    Idle = 0,
    Recording = 1,
    Paused = 2,
    /// Start requested (Voice Notes may be launching or loading its model).
    Starting = 3,
    /// Stop requested (Voice Notes is finishing the note).
    Stopping = 4,
}

/// NOTES_STATE `notice`: a one-off event for a toast on the Device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NotesNotice {
    None = 0,
    Started = 1,
    Stopped = 2,
    LaunchFailed = 3,
    StartFailed = 4,
    RiskBluetooth = 5,
    RiskOther = 6,
    NotInstalled = 7,
    RiskVoiceIsolation = 8,
    ControlDisabled = 9,
    StopFailed = 10,
}

/// ACTION_RESULT action codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Action {
    Submit = 0x20,
    Undo = 0x21,
}

/// One AUDIO frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    pub dict: u8,
    pub seq: u16,
    pub pred: i16,
    pub index: u8,
    pub adpcm: Vec<u8>,
}

/// A frame sent by the Device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceFrame {
    Hello {
        ver: u8,
        fw: String,
    },
    DictStart {
        dict: u8,
    },
    Audio(AudioFrame),
    DictStop {
        dict: u8,
    },
    DictCancel {
        dict: u8,
    },
    Submit,
    Undo,
    TargetsReq {
        list: u8,
    },
    TargetSelect {
        list: u8,
        index: u8,
    },
    /// Start or stop a Voice Notes Recording.
    NotesToggle,
    /// Open an Alert: Jump to its session.
    AlertOpen {
        id: u8,
    },
    /// Drop an Alert.
    AlertDismiss {
        id: u8,
    },
}

/// A frame sent by the Companion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompanionFrame {
    HelloAck {
        ver: u8,
    },
    Status {
        code: StatusCode,
        text: String,
    },
    Partial {
        dict: u8,
        text: String,
    },
    Result {
        dict: u8,
        status: Status,
        text: String,
    },
    ActionResult {
        action: Action,
        status: Status,
    },
    TargetItem {
        list: u8,
        index: u8,
        count: u8,
        flags: u8,
        label: String,
    },
    TargetEnd {
        list: u8,
        count: u8,
    },
    TargetState {
        status: Status,
        kind: TargetKind,
        /// Supported App index (0 Orca, 1 WeChat, 2 ChatGPT, 3 WeCom) or
        /// [`APP_NONE`].
        app: u8,
        label: String,
    },
    /// An Orca agent session waits for the user.
    Alert {
        id: u8,
        /// Supported App index (0 = Orca).
        app: u8,
        label: String,
        message: String,
    },
    AlertClear {
        id: u8,
    },
    /// The next part of an Alert's message, at byte `offset` of the message.
    AlertMore {
        id: u8,
        offset: u16,
        text: String,
    },
    NotesState {
        state: NotesState,
        /// Recording time so far (paused time excluded).
        elapsed_s: u32,
        notice: NotesNotice,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Empty,
    TooLong(usize),
    UnknownType(u8),
    Truncated { ty: u8, len: usize },
    BadUtf8 { ty: u8 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Empty => write!(f, "empty frame"),
            DecodeError::TooLong(n) => write!(f, "frame of {n} bytes exceeds {MAX_FRAME}"),
            DecodeError::UnknownType(t) => write!(f, "unknown frame type 0x{t:02x}"),
            DecodeError::Truncated { ty, len } => {
                write!(f, "frame 0x{ty:02x} truncated ({len} bytes)")
            }
            DecodeError::BadUtf8 { ty } => write!(f, "frame 0x{ty:02x} has invalid UTF-8"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Return the longest tail of `text` that fits in `max` bytes and starts on a
/// UTF-8 character boundary.
pub fn utf8_tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Return the longest head of `text` that fits in `max` bytes, cut on a UTF-8
/// character boundary.
pub fn utf8_head(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn need(bytes: &[u8], len: usize) -> Result<(), DecodeError> {
    if bytes.len() < len {
        Err(DecodeError::Truncated {
            ty: bytes[0],
            len: bytes.len(),
        })
    } else {
        Ok(())
    }
}

impl DeviceFrame {
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.is_empty() {
            return Err(DecodeError::Empty);
        }
        if bytes.len() > MAX_FRAME {
            return Err(DecodeError::TooLong(bytes.len()));
        }
        let t = bytes[0];
        Ok(match t {
            ty::HELLO => {
                need(bytes, 2)?;
                let fw = std::str::from_utf8(&bytes[2..])
                    .map_err(|_| DecodeError::BadUtf8 { ty: t })?
                    .to_owned();
                DeviceFrame::Hello { ver: bytes[1], fw }
            }
            ty::DICT_START => {
                need(bytes, 2)?;
                DeviceFrame::DictStart { dict: bytes[1] }
            }
            ty::AUDIO => {
                need(bytes, 7 + ADPCM_BYTES)?;
                DeviceFrame::Audio(AudioFrame {
                    dict: bytes[1],
                    seq: u16::from_le_bytes([bytes[2], bytes[3]]),
                    pred: i16::from_le_bytes([bytes[4], bytes[5]]),
                    index: bytes[6],
                    adpcm: bytes[7..7 + ADPCM_BYTES].to_vec(),
                })
            }
            ty::DICT_STOP => {
                need(bytes, 2)?;
                DeviceFrame::DictStop { dict: bytes[1] }
            }
            ty::DICT_CANCEL => {
                need(bytes, 2)?;
                DeviceFrame::DictCancel { dict: bytes[1] }
            }
            ty::SUBMIT => DeviceFrame::Submit,
            ty::UNDO => DeviceFrame::Undo,
            ty::TARGETS_REQ => {
                need(bytes, 2)?;
                DeviceFrame::TargetsReq { list: bytes[1] }
            }
            ty::TARGET_SELECT => {
                need(bytes, 3)?;
                DeviceFrame::TargetSelect {
                    list: bytes[1],
                    index: bytes[2],
                }
            }
            ty::NOTES_TOGGLE => DeviceFrame::NotesToggle,
            ty::ALERT_OPEN => {
                need(bytes, 2)?;
                DeviceFrame::AlertOpen { id: bytes[1] }
            }
            ty::ALERT_DISMISS => {
                need(bytes, 2)?;
                DeviceFrame::AlertDismiss { id: bytes[1] }
            }
            other => return Err(DecodeError::UnknownType(other)),
        })
    }

    /// Encode (used by tests and the simulator to play the Device role).
    pub fn encode(&self) -> Vec<u8> {
        match self {
            DeviceFrame::Hello { ver, fw } => {
                let mut v = vec![ty::HELLO, *ver];
                v.extend_from_slice(utf8_head(fw, MAX_FRAME - 2).as_bytes());
                v
            }
            DeviceFrame::DictStart { dict } => vec![ty::DICT_START, *dict],
            DeviceFrame::Audio(a) => {
                let mut v = Vec::with_capacity(7 + ADPCM_BYTES);
                v.push(ty::AUDIO);
                v.push(a.dict);
                v.extend_from_slice(&a.seq.to_le_bytes());
                v.extend_from_slice(&a.pred.to_le_bytes());
                v.push(a.index);
                v.extend_from_slice(&a.adpcm);
                v
            }
            DeviceFrame::DictStop { dict } => vec![ty::DICT_STOP, *dict],
            DeviceFrame::DictCancel { dict } => vec![ty::DICT_CANCEL, *dict],
            DeviceFrame::Submit => vec![ty::SUBMIT],
            DeviceFrame::Undo => vec![ty::UNDO],
            DeviceFrame::TargetsReq { list } => vec![ty::TARGETS_REQ, *list],
            DeviceFrame::TargetSelect { list, index } => vec![ty::TARGET_SELECT, *list, *index],
            DeviceFrame::NotesToggle => vec![ty::NOTES_TOGGLE],
            DeviceFrame::AlertOpen { id } => vec![ty::ALERT_OPEN, *id],
            DeviceFrame::AlertDismiss { id } => vec![ty::ALERT_DISMISS, *id],
        }
    }
}

fn with_text(mut head: Vec<u8>, text: &str) -> Vec<u8> {
    let room = MAX_FRAME - head.len();
    head.extend_from_slice(utf8_tail(text, room).as_bytes());
    head
}

fn status_from(b: u8) -> Option<Status> {
    Some(match b {
        0 => Status::Ok,
        1 => Status::Empty,
        2 => Status::Cancelled,
        3 => Status::TargetUnavailable,
        4 => Status::RecognizerError,
        5 => Status::Permission,
        6 => Status::NothingToUndo,
        _ => return None,
    })
}

/// An Alert as frames: ALERT with the label and the head of the message,
/// then ALERT_MORE frames with the rest, each cut on a UTF-8 boundary.
pub fn alert_frames(id: u8, app: u8, label: &str, message: &str) -> Vec<CompanionFrame> {
    let label = utf8_head(label, 63);
    let head_room = MAX_FRAME - 4 - label.len();
    let head = utf8_head(message, head_room);
    let mut frames = vec![CompanionFrame::Alert {
        id,
        app,
        label: label.to_owned(),
        message: head.to_owned(),
    }];
    let mut offset = head.len();
    while offset < message.len() && offset <= usize::from(u16::MAX) {
        let part = utf8_head(&message[offset..], MAX_FRAME - 4);
        if part.is_empty() {
            break;
        }
        frames.push(CompanionFrame::AlertMore {
            id,
            offset: offset as u16,
            text: part.to_owned(),
        });
        offset += part.len();
    }
    frames
}

impl CompanionFrame {
    /// Encode to at most [`MAX_FRAME`] bytes. Text fields are cut to their tail.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            CompanionFrame::HelloAck { ver } => vec![ty::HELLO_ACK, *ver],
            CompanionFrame::Status { code, text } => with_text(vec![ty::STATUS, *code as u8], text),
            CompanionFrame::Partial { dict, text } => with_text(vec![ty::PARTIAL, *dict], text),
            CompanionFrame::Result { dict, status, text } => {
                with_text(vec![ty::RESULT, *dict, *status as u8], text)
            }
            CompanionFrame::ActionResult { action, status } => {
                vec![ty::ACTION_RESULT, *action as u8, *status as u8]
            }
            CompanionFrame::TargetItem {
                list,
                index,
                count,
                flags,
                label,
            } => with_text(vec![ty::TARGET_ITEM, *list, *index, *count, *flags], label),
            CompanionFrame::TargetEnd { list, count } => vec![ty::TARGET_END, *list, *count],
            CompanionFrame::TargetState {
                status,
                kind,
                app,
                label,
            } => with_text(
                vec![ty::TARGET_STATE, *status as u8, *kind as u8, *app],
                label,
            ),
            CompanionFrame::Alert {
                id,
                app,
                label,
                message,
            } => {
                // Label first (its length byte), then the message in the rest.
                let label = utf8_head(label, 63);
                let mut v = vec![ty::ALERT, *id, *app, label.len() as u8];
                v.extend_from_slice(label.as_bytes());
                let room = MAX_FRAME - v.len();
                v.extend_from_slice(utf8_head(message, room).as_bytes());
                v
            }
            CompanionFrame::AlertClear { id } => vec![ty::ALERT_CLEAR, *id],
            CompanionFrame::AlertMore { id, offset, text } => {
                let mut v = vec![ty::ALERT_MORE, *id];
                v.extend_from_slice(&offset.to_le_bytes());
                let room = MAX_FRAME - v.len();
                v.extend_from_slice(utf8_head(text, room).as_bytes());
                v
            }
            CompanionFrame::NotesState {
                state,
                elapsed_s,
                notice,
            } => {
                let mut v = vec![ty::NOTES_STATE, *state as u8];
                v.extend_from_slice(&elapsed_s.to_le_bytes());
                v.push(*notice as u8);
                v
            }
        }
    }

    /// Decode (used by tests and the simulator to play the Device role).
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.is_empty() {
            return Err(DecodeError::Empty);
        }
        if bytes.len() > MAX_FRAME {
            return Err(DecodeError::TooLong(bytes.len()));
        }
        let t = bytes[0];
        let text = |from: usize| -> Result<String, DecodeError> {
            std::str::from_utf8(&bytes[from..])
                .map(str::to_owned)
                .map_err(|_| DecodeError::BadUtf8 { ty: t })
        };
        let st = |b: u8| {
            status_from(b).ok_or(DecodeError::Truncated {
                ty: t,
                len: bytes.len(),
            })
        };
        Ok(match t {
            ty::HELLO_ACK => {
                need(bytes, 2)?;
                CompanionFrame::HelloAck { ver: bytes[1] }
            }
            ty::STATUS => {
                need(bytes, 2)?;
                let code = match bytes[1] {
                    0 => StatusCode::Clear,
                    1 => StatusCode::SpeechPermission,
                    2 => StatusCode::AccessibilityPermission,
                    3 => StatusCode::OrcaUnavailable,
                    4 => StatusCode::RecognizerUnavailable,
                    _ => {
                        return Err(DecodeError::Truncated {
                            ty: t,
                            len: bytes.len(),
                        });
                    }
                };
                CompanionFrame::Status {
                    code,
                    text: text(2)?,
                }
            }
            ty::PARTIAL => {
                need(bytes, 2)?;
                CompanionFrame::Partial {
                    dict: bytes[1],
                    text: text(2)?,
                }
            }
            ty::RESULT => {
                need(bytes, 3)?;
                CompanionFrame::Result {
                    dict: bytes[1],
                    status: st(bytes[2])?,
                    text: text(3)?,
                }
            }
            ty::ACTION_RESULT => {
                need(bytes, 3)?;
                let action = match bytes[1] {
                    0x20 => Action::Submit,
                    0x21 => Action::Undo,
                    _ => {
                        return Err(DecodeError::Truncated {
                            ty: t,
                            len: bytes.len(),
                        });
                    }
                };
                CompanionFrame::ActionResult {
                    action,
                    status: st(bytes[2])?,
                }
            }
            ty::TARGET_ITEM => {
                need(bytes, 5)?;
                CompanionFrame::TargetItem {
                    list: bytes[1],
                    index: bytes[2],
                    count: bytes[3],
                    flags: bytes[4],
                    label: text(5)?,
                }
            }
            ty::TARGET_END => {
                need(bytes, 3)?;
                CompanionFrame::TargetEnd {
                    list: bytes[1],
                    count: bytes[2],
                }
            }
            ty::TARGET_STATE => {
                need(bytes, 4)?;
                let kind = match bytes[2] {
                    0 => TargetKind::Unknown,
                    1 => TargetKind::App,
                    2 => TargetKind::Orca,
                    _ => {
                        return Err(DecodeError::Truncated {
                            ty: t,
                            len: bytes.len(),
                        });
                    }
                };
                CompanionFrame::TargetState {
                    status: st(bytes[1])?,
                    kind,
                    app: bytes[3],
                    label: text(4)?,
                }
            }
            ty::ALERT => {
                need(bytes, 4)?;
                let n = usize::from(bytes[3]);
                if 4 + n > bytes.len() {
                    return Err(DecodeError::Truncated {
                        ty: t,
                        len: bytes.len(),
                    });
                }
                let label = std::str::from_utf8(&bytes[4..4 + n])
                    .map_err(|_| DecodeError::BadUtf8 { ty: t })?
                    .to_owned();
                CompanionFrame::Alert {
                    id: bytes[1],
                    app: bytes[2],
                    label,
                    message: text(4 + n)?,
                }
            }
            ty::ALERT_CLEAR => {
                need(bytes, 2)?;
                CompanionFrame::AlertClear { id: bytes[1] }
            }
            ty::ALERT_MORE => {
                need(bytes, 4)?;
                CompanionFrame::AlertMore {
                    id: bytes[1],
                    offset: u16::from_le_bytes([bytes[2], bytes[3]]),
                    text: text(4)?,
                }
            }
            ty::NOTES_STATE => {
                need(bytes, 7)?;
                let bad = DecodeError::Truncated {
                    ty: t,
                    len: bytes.len(),
                };
                let state = match bytes[1] {
                    0 => NotesState::Idle,
                    1 => NotesState::Recording,
                    2 => NotesState::Paused,
                    3 => NotesState::Starting,
                    4 => NotesState::Stopping,
                    _ => return Err(bad),
                };
                let notice = match bytes[6] {
                    0 => NotesNotice::None,
                    1 => NotesNotice::Started,
                    2 => NotesNotice::Stopped,
                    3 => NotesNotice::LaunchFailed,
                    4 => NotesNotice::StartFailed,
                    5 => NotesNotice::RiskBluetooth,
                    6 => NotesNotice::RiskOther,
                    7 => NotesNotice::NotInstalled,
                    8 => NotesNotice::RiskVoiceIsolation,
                    9 => NotesNotice::ControlDisabled,
                    10 => NotesNotice::StopFailed,
                    _ => return Err(bad),
                };
                CompanionFrame::NotesState {
                    state,
                    elapsed_s: u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]),
                    notice,
                }
            }
            other => return Err(DecodeError::UnknownType(other)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_respects_utf8_boundaries() {
        let s = "把这个函数改成异步"; // 9 chars * 3 bytes
        assert_eq!(utf8_tail(s, 100), s);
        assert_eq!(utf8_tail(s, 6), "异步");
        assert_eq!(utf8_tail(s, 7), "异步");
        assert_eq!(utf8_tail(s, 8), "异步");
        assert_eq!(utf8_tail(s, 9), "成异步");
        assert_eq!(utf8_tail(s, 2), "");
        assert_eq!(utf8_tail("abc", 2), "bc");
        assert_eq!(utf8_head(s, 7), "把这");
    }

    #[test]
    fn long_partial_is_cut_to_tail_within_limit() {
        let text: String = "测试".repeat(100) + "结尾";
        let bytes = CompanionFrame::Partial {
            dict: 7,
            text: text.clone(),
        }
        .encode();
        assert!(bytes.len() <= MAX_FRAME);
        assert_eq!(bytes[0], ty::PARTIAL);
        assert_eq!(bytes[1], 7);
        let body = std::str::from_utf8(&bytes[2..]).unwrap();
        assert!(text.ends_with(body));
        assert!(body.ends_with("结尾"));
        // 178 bytes of room; 3-byte chars → 59 chars = 177 bytes.
        assert_eq!(body.len(), 177);
    }

    #[test]
    fn mixed_width_tail_never_splits() {
        for n in 0..10 {
            let text = format!("{}{}", "a".repeat(n), "é中🎉".repeat(40));
            for f in [
                CompanionFrame::Result {
                    dict: 1,
                    status: Status::Ok,
                    text: text.clone(),
                },
                CompanionFrame::Status {
                    code: StatusCode::Clear,
                    text: text.clone(),
                },
                CompanionFrame::TargetItem {
                    list: 0,
                    index: 0,
                    count: 1,
                    flags: 0,
                    label: text.clone(),
                },
                CompanionFrame::TargetState {
                    status: Status::Ok,
                    kind: TargetKind::App,
                    app: 1,
                    label: text.clone(),
                },
            ] {
                let b = f.encode();
                assert!(b.len() <= MAX_FRAME);
                let back = CompanionFrame::decode(&b).expect("valid utf8 after cut");
                let _ = back;
            }
        }
    }

    #[test]
    fn companion_frames_byte_layout() {
        assert_eq!(CompanionFrame::HelloAck { ver: 2 }.encode(), vec![0x81, 2]);
        assert_eq!(
            CompanionFrame::Status {
                code: StatusCode::AccessibilityPermission,
                text: "AX".into()
            }
            .encode(),
            vec![0x82, 2, b'A', b'X']
        );
        assert_eq!(
            CompanionFrame::Result {
                dict: 3,
                status: Status::Cancelled,
                text: String::new()
            }
            .encode(),
            vec![0x91, 3, 2]
        );
        assert_eq!(
            CompanionFrame::ActionResult {
                action: Action::Undo,
                status: Status::NothingToUndo
            }
            .encode(),
            vec![0xA0, 0x21, 6]
        );
        assert_eq!(
            CompanionFrame::TargetItem {
                list: 1,
                index: 2,
                count: 5,
                flags: FLAG_CURRENT | FLAG_NOT_RUNNING,
                label: "x".into()
            }
            .encode(),
            vec![0xB0, 1, 2, 5, 5, b'x']
        );
        assert_eq!(
            CompanionFrame::TargetEnd { list: 0, count: 8 }.encode(),
            vec![0xB1, 0, 8]
        );
        assert_eq!(
            CompanionFrame::TargetState {
                status: Status::TargetUnavailable,
                kind: TargetKind::Orca,
                app: 0,
                label: "o".into()
            }
            .encode(),
            vec![0xB2, 3, 2, 0, b'o']
        );
    }

    #[test]
    fn alert_frames_layout() {
        assert_eq!(DeviceFrame::AlertOpen { id: 7 }.encode(), vec![0x50, 7]);
        assert_eq!(
            DeviceFrame::decode(&[0x51, 9]).unwrap(),
            DeviceFrame::AlertDismiss { id: 9 }
        );
        let f = CompanionFrame::Alert {
            id: 3,
            app: 0,
            label: "wt · 修复".into(),
            message: "好了".into(),
        };
        let b = f.encode();
        assert_eq!(&b[..4], &[0xD0, 3, 0, "wt · 修复".len() as u8]);
        assert_eq!(CompanionFrame::decode(&b).unwrap(), f);
        // Never over 180 bytes, cut on character boundaries.
        let big = CompanionFrame::Alert {
            id: 1,
            app: 0,
            label: "标".repeat(40),
            message: "消".repeat(80),
        };
        let b = big.encode();
        assert!(b.len() <= MAX_FRAME);
        let CompanionFrame::Alert { label, message, .. } = CompanionFrame::decode(&b).unwrap()
        else {
            panic!()
        };
        assert_eq!(label, "标".repeat(21));
        assert!(!message.is_empty());
        assert_eq!(CompanionFrame::AlertClear { id: 4 }.encode(), vec![0xD1, 4]);
        // Long messages continue in ALERT_MORE frames that rebuild the text.
        let msg = "第".repeat(110) + "end";
        let frames = alert_frames(5, 0, "wt · task", &msg);
        assert!(frames.len() >= 2);
        let mut rebuilt = String::new();
        for f in &frames {
            let b = f.encode();
            assert!(b.len() <= MAX_FRAME);
            match CompanionFrame::decode(&b).unwrap() {
                CompanionFrame::Alert { message, .. } => rebuilt.push_str(&message),
                CompanionFrame::AlertMore { id, offset, text } => {
                    assert_eq!((id, usize::from(offset)), (5, rebuilt.len()));
                    rebuilt.push_str(&text);
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(rebuilt, msg);
        assert_eq!(alert_frames(1, 0, "l", "short").len(), 1);
        assert_eq!(
            CompanionFrame::AlertMore {
                id: 2,
                offset: 300,
                text: "x".into()
            }
            .encode(),
            vec![0xD2, 2, 44, 1, b'x']
        );
        assert!(CompanionFrame::decode(&[0xD0, 1, 0, 9, b'a']).is_err());
    }

    #[test]
    fn notes_frames_layout() {
        assert_eq!(DeviceFrame::NotesToggle.encode(), vec![0x40]);
        assert_eq!(
            DeviceFrame::decode(&[0x40]).unwrap(),
            DeviceFrame::NotesToggle
        );
        let f = CompanionFrame::NotesState {
            state: NotesState::Recording,
            elapsed_s: 0x0102_0304,
            notice: NotesNotice::RiskBluetooth,
        };
        assert_eq!(f.encode(), vec![0xC0, 1, 4, 3, 2, 1, 5]);
        assert_eq!(CompanionFrame::decode(&f.encode()).unwrap(), f);
        assert!(CompanionFrame::decode(&[0xC0, 1, 0, 0, 0, 0]).is_err());
        assert!(CompanionFrame::decode(&[0xC0, 9, 0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn companion_roundtrip() {
        let frames = vec![
            CompanionFrame::HelloAck { ver: 2 },
            CompanionFrame::Partial {
                dict: 255,
                text: "你好".into(),
            },
            CompanionFrame::Result {
                dict: 0,
                status: Status::Ok,
                text: "完成".into(),
            },
            CompanionFrame::ActionResult {
                action: Action::Submit,
                status: Status::Ok,
            },
            CompanionFrame::TargetEnd { list: 1, count: 0 },
            CompanionFrame::TargetState {
                status: Status::Ok,
                kind: TargetKind::App,
                app: 3,
                label: "企业微信 · 群".into(),
            },
        ];
        for f in frames {
            assert_eq!(CompanionFrame::decode(&f.encode()).unwrap(), f);
        }
    }

    #[test]
    fn device_frames_decode() {
        assert_eq!(
            DeviceFrame::decode(&[0x01, 1, b'f', b'w']).unwrap(),
            DeviceFrame::Hello {
                ver: 1,
                fw: "fw".into()
            }
        );
        assert_eq!(
            DeviceFrame::decode(&[0x10, 9]).unwrap(),
            DeviceFrame::DictStart { dict: 9 }
        );
        assert_eq!(
            DeviceFrame::decode(&[0x12, 9]).unwrap(),
            DeviceFrame::DictStop { dict: 9 }
        );
        assert_eq!(
            DeviceFrame::decode(&[0x13, 9]).unwrap(),
            DeviceFrame::DictCancel { dict: 9 }
        );
        assert_eq!(DeviceFrame::decode(&[0x20]).unwrap(), DeviceFrame::Submit);
        assert_eq!(DeviceFrame::decode(&[0x21]).unwrap(), DeviceFrame::Undo);
        assert_eq!(
            DeviceFrame::decode(&[0x30, 1]).unwrap(),
            DeviceFrame::TargetsReq { list: 1 }
        );
        assert_eq!(
            DeviceFrame::decode(&[0x31, 0, 4]).unwrap(),
            DeviceFrame::TargetSelect { list: 0, index: 4 }
        );
        assert_eq!(DeviceFrame::decode(&[]), Err(DecodeError::Empty));
        assert_eq!(
            DeviceFrame::decode(&[0x55]),
            Err(DecodeError::UnknownType(0x55))
        );
        assert!(matches!(
            DeviceFrame::decode(&[0x31, 0]),
            Err(DecodeError::Truncated { .. })
        ));
        assert!(matches!(
            DeviceFrame::decode(&[0x01, 1, 0xff]),
            Err(DecodeError::BadUtf8 { .. })
        ));
        assert!(matches!(
            DeviceFrame::decode(&[0x20; 181]),
            Err(DecodeError::TooLong(181))
        ));
    }

    #[test]
    fn audio_frame_layout() {
        let mut raw = vec![0x11, 5, 0x34, 0x12, 0xfe, 0xff, 42];
        raw.extend((0..160).map(|i| i as u8));
        assert_eq!(raw.len(), 167);
        match DeviceFrame::decode(&raw).unwrap() {
            DeviceFrame::Audio(a) => {
                assert_eq!(a.dict, 5);
                assert_eq!(a.seq, 0x1234);
                assert_eq!(a.pred, -2);
                assert_eq!(a.index, 42);
                assert_eq!(a.adpcm.len(), 160);
                assert_eq!(a.adpcm[159], 159);
                assert_eq!(DeviceFrame::Audio(a).encode(), raw);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            DeviceFrame::decode(&raw[..100]),
            Err(DecodeError::Truncated { .. })
        ));
    }
}
