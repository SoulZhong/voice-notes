//! 设备双击 OK 开始/停止 Voice Notes 录音:进程内直调 recording_ctl。
//! 原来独立配套程序走 mcp.sock(Unix socket),搬进来后不再绕本地接口,
//! 也就绕开了 Windows 上本地控制接口还是空壳的问题。

use vibe_device::notes::{NotesError, NotesPhase, NotesStatus, VoiceNotesApi};

pub struct InProcessNotes {
    pub app: tauri::AppHandle,
}

fn parse_status(v: &serde_json::Value) -> NotesStatus {
    let phase = match v.get("state").and_then(|s| s.as_str()) {
        Some("recording") => NotesPhase::Recording,
        Some("paused") => NotesPhase::Paused,
        _ => NotesPhase::Idle,
    };
    NotesStatus {
        phase,
        elapsed_ms: v.get("elapsed_ms").and_then(|e| e.as_u64()).unwrap_or(0),
    }
}

fn risk_kinds(v: &serde_json::Value) -> Vec<String> {
    v.get("risks")
        .and_then(|r| r.as_array())
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("kind").and_then(|k| k.as_str()).map(str::to_owned))
        .collect()
}

impl VoiceNotesApi for InProcessNotes {
    fn status(&mut self) -> Result<NotesStatus, NotesError> {
        Ok(parse_status(&crate::recording_ctl::status(&self.app)))
    }

    fn start(&mut self) -> Result<Vec<String>, NotesError> {
        if self.status()?.phase != NotesPhase::Idle {
            return Ok(Vec::new()); // 已在录:没什么要开的
        }
        crate::recording_ctl::start(&self.app, None)
            .map(|v| risk_kinds(&v))
            .map_err(NotesError::Failed)
    }

    fn stop(&mut self) -> Result<(), NotesError> {
        match crate::recording_ctl::stop(&self.app) {
            Ok(_) => Ok(()),
            Err(m) if m.contains("没有正在进行的录制") => Ok(()),
            Err(m) => Err(NotesError::Failed(m)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_and_risks_parse_the_control_json() {
        let v = serde_json::json!({"state": "paused", "elapsed_ms": 4200});
        assert_eq!(
            parse_status(&v),
            NotesStatus { phase: NotesPhase::Paused, elapsed_ms: 4200 }
        );
        assert_eq!(parse_status(&serde_json::json!({"state": "idle"})).phase, NotesPhase::Idle);
        let v = serde_json::json!({"note_id": "x", "risks": [{"kind": "bluetooth_mic", "message": "…"}]});
        assert_eq!(risk_kinds(&v), ["bluetooth_mic"]);
        assert!(risk_kinds(&serde_json::json!({})).is_empty());
    }
}
