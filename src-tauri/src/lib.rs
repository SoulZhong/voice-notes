mod audio;
pub mod pipeline;
pub mod asr;
mod ipc;
mod session;

use std::sync::{Arc, Mutex};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, State};

use audio::{AudioCapture, Source};
use pipeline::segmenter::Segmenter;
use session::RecordingHandle;

#[derive(Default)]
struct AppState {
    running: Arc<Mutex<bool>>,
    handle: Arc<Mutex<Option<RecordingHandle>>>,
}

fn models_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models")
}

fn new_silero(vad_path: &std::path::Path) -> anyhow::Result<Box<dyn Segmenter>> {
    Ok(Box::new(pipeline::silero::SileroSegmenter::new(vad_path)?) as Box<dyn Segmenter>)
}

/// 从 failed 列表把 System 的失败归类为 "denied"（未授权）/ "unavailable"（其它）。
fn classify_system(active: &[Source], failed: &[(Source, String)]) -> String {
    if active.contains(&Source::System) {
        return "on".into();
    }
    match failed.iter().find(|(s, _)| *s == Source::System) {
        Some((_, msg)) if msg.contains("unauthorized") => "denied".into(),
        Some(_) => "unavailable".into(),
        None => "unavailable".into(),
    }
}

#[tauri::command]
fn start_recording(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    {
        let mut r = state.running.lock().unwrap();
        if *r {
            return Err("已在录制".into());
        }
        *r = true;
    }
    let running = state.running.clone();
    let handle_slot = state.handle.clone();

    std::thread::spawn(move || {
        let fail = |app: &AppHandle, running: &Arc<Mutex<bool>>, msg: String| {
            let _ = app.emit("status", ipc::StatusEvent { state: msg, system_audio: String::new() });
            *running.lock().unwrap() = false;
        };

        // 1) 先建 recognizer（加载模型，耗时）——就绪后才发 recording，消除闪烁。
        let sv_dir = models_dir().join("sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17");
        let recognizer = match asr::sense_voice::SenseVoiceRecognizer::new(&sv_dir) {
            Ok(r) => Box::new(r) as Box<dyn asr::Recognizer>,
            Err(e) => return fail(&app, &running, format!("error: {e}")),
        };

        // 2) 构建两路源（各自 VAD）。麦克风必备；系统声音失败则由 start_session 降级。
        let vad_path = models_dir().join("silero_vad.onnx");
        let mic_seg = match new_silero(&vad_path) {
            Ok(s) => s,
            Err(e) => return fail(&app, &running, format!("error: {e}")),
        };
        let mut sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> = vec![(
            Source::Mic,
            Box::new(audio::microphone::Microphone::new()),
            mic_seg,
        )];

        #[cfg(target_os = "macos")]
        {
            match new_silero(&vad_path) {
                Ok(sys_seg) => sources.push((
                    Source::System,
                    Box::new(audio::system::SystemAudioCapture::new()),
                    sys_seg,
                )),
                Err(e) => {
                    let _ = app.emit(
                        "status",
                        ipc::StatusEvent { state: format!("error: {e}"), system_audio: String::new() },
                    );
                }
            }
        }

        // 3) 起会话。emit 回调带 source 字符串。
        let app_f = app.clone();
        let app_p = app.clone();
        let start = session::start_session(
            sources,
            recognizer,
            16000,
            16000,
            move |src, text| {
                let _ = app_f.emit(
                    "final",
                    ipc::FinalEvent { source: src.as_str().into(), text },
                );
            },
            move |src, text| {
                let _ = app_p.emit(
                    "partial",
                    ipc::PartialEvent { source: src.as_str().into(), text },
                );
            },
        );

        match start {
            Ok(start) => {
                let system_audio = classify_system(&start.active, &start.failed);
                *handle_slot.lock().unwrap() = Some(start.handle);
                let _ = app.emit(
                    "status",
                    ipc::StatusEvent { state: "recording".into(), system_audio },
                );
            }
            Err(e) => return fail(&app, &running, format!("error: {e}")),
        }
    });

    Ok(())
}

#[tauri::command]
fn stop_recording(app: AppHandle, state: State<AppState>) {
    // 真停止：取出句柄并优雅停止（停 capture → flush 尾段 → 排干 finals → join）。
    let handle = state.handle.lock().unwrap().take();
    if let Some(h) = handle {
        h.stop();
    }
    *state.running.lock().unwrap() = false;
    let _ = app.emit(
        "status",
        ipc::StatusEvent { state: "stopped".into(), system_audio: String::new() },
    );
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![start_recording, stop_recording])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
