mod audio;
mod pipeline;
pub mod asr;
mod ipc;
mod session;

use std::sync::{Arc, Mutex};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, State};

#[derive(Default)]
struct AppState {
    running: Arc<Mutex<bool>>,
}

#[tauri::command]
fn start_recording(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    {
        let mut r = state.running.lock().unwrap();
        if *r { return Err("已在录制".into()); }
        *r = true;
    }
    let running = state.running.clone();
    let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("models/sherpa-onnx-whisper-base");

    std::thread::spawn(move || {
        let _ = app.emit("status", ipc::StatusEvent { state: "recording".into() });
        let recognizer = match asr::whisper::WhisperRecognizer::new(&model_dir) {
            Ok(r) => Box::new(r) as Box<dyn asr::Recognizer>,
            Err(e) => {
                let _ = app.emit("status", ipc::StatusEvent { state: format!("error: {e}") });
                *running.lock().unwrap() = false;
                return;
            }
        };
        let capture = Box::new(audio::microphone::Microphone::new()) as Box<dyn audio::AudioCapture>;
        let app2 = app.clone();
        if let Err(e) = session::run_pipeline(capture, recognizer, 16000, 1.5, move |text| {
            let _ = app2.emit("partial", ipc::PartialEvent { text });
        }) {
            let _ = app.emit("status", ipc::StatusEvent { state: format!("error: {e}") });
            *running.lock().unwrap() = false;
            return;
        }
        *running.lock().unwrap() = false;
        let _ = app.emit("status", ipc::StatusEvent { state: "stopped".into() });
    });
    Ok(())
}

#[tauri::command]
fn stop_recording(state: State<AppState>) {
    // 骨架：置 false 并依赖关闭设备停止；完整停止逻辑在后续计划完善。
    *state.running.lock().unwrap() = false;
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
