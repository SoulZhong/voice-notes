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

// 锁序约定（必须在任何持锁场景下遵守）：running → generation → handle_slot。
// 只有 start_recording 的加载线程会嵌套持有 running→generation（以及 running→
// generation→handle_slot），且只在极短的检查/存储语句内完成；stop_recording
// 每条语句只持有一把锁，从不同时持有两把，因此不存在死锁风险。
//
// generation 协议：stop_recording 和每次新的 start_recording 都会递增
// generation。加载线程在耗时的模型/会话初始化完成后，无论是要存 handle
// （成功路径）还是要清空 running（失败路径 fail()），都必须先确认自己捕获的
// my_gen 仍然等于当前 generation —— 只有仍是"当前代"时，才允许改动共享状态；
// 否则说明该线程是被后续 stop/start 抢先淘汰的过期加载，直接静默让路，避免
// 已被覆盖或已被终止的会话把自己的（过期的）结果错误地写回全局状态。
#[derive(Default)]
struct AppState {
    running: Arc<Mutex<bool>>,
    generation: Arc<Mutex<u64>>,
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
    let my_gen = {
        let mut r = state.running.lock().unwrap();
        if *r {
            return Err("已在录制".into());
        }
        *r = true;
        // 锁序 running → generation：running 锁仍持有时嵌套锁 generation 并
        // 递增，捕获的 my_gen 即本次会话的"代号"，随线程一起移动。
        let mut g = state.generation.lock().unwrap();
        *g += 1;
        *g
    };
    let running = state.running.clone();
    let generation = state.generation.clone();
    let handle_slot = state.handle.clone();

    std::thread::spawn(move || {
        // fail()：加载过程中任何一步失败都会调用。必须先确认 my_gen 仍是当前代
        // 才清空 running / 发出 error —— 否则说明本线程已过期（被后续的
        // stop/start 淘汰），其失败结果不该覆盖更新的会话状态，静默丢弃即可。
        let fail = |app: &AppHandle, running: &Arc<Mutex<bool>>, generation: &Arc<Mutex<u64>>, my_gen: u64, msg: String| {
            let running_guard = running.lock().unwrap();
            let gen_guard = generation.lock().unwrap();
            if *gen_guard != my_gen {
                drop(gen_guard);
                drop(running_guard);
                eprintln!("过期加载线程的失败被忽略: {msg}");
                return;
            }
            drop(gen_guard);
            let mut running_guard = running_guard;
            *running_guard = false;
            drop(running_guard);
            let _ = app.emit("status", ipc::StatusEvent { state: msg, system_audio: String::new() });
        };

        // 1) 先建 recognizer（加载模型，耗时）——就绪后才发 recording，消除闪烁。
        let sv_dir = models_dir().join("sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17");
        let recognizer = match asr::sense_voice::SenseVoiceRecognizer::new(&sv_dir) {
            Ok(r) => Box::new(r) as Box<dyn asr::Recognizer>,
            Err(e) => return fail(&app, &running, &generation, my_gen, format!("error: {e}")),
        };

        // 2) 构建两路源（各自 VAD）。麦克风必备；系统声音失败则由 start_session 降级。
        let vad_path = models_dir().join("silero_vad.onnx");
        let mic_seg = match new_silero(&vad_path) {
            Ok(s) => s,
            Err(e) => return fail(&app, &running, &generation, my_gen, format!("error: {e}")),
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
                    // 系统声音 VAD 构建失败非致命：不发 error 状态（避免闪烁），
                    // 静默跳过该源；classify_system 会因 System 既不在 active 也不在
                    // failed 里而归类为 "unavailable"，UI 仍会显示降级横幅。
                    eprintln!("系统声音 VAD 构建失败，降级为仅麦克风: {e}");
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
                // Fix A: mic is mandatory — if it failed to start, tear down and surface as error.
                if !start.active.contains(&Source::Mic) {
                    start.handle.stop();
                    let mic_err = start.failed.iter()
                        .find(|(s, _)| *s == Source::Mic)
                        .map(|(_, msg)| format!("error: 麦克风未能启动: {msg}"))
                        .unwrap_or_else(|| "error: 麦克风未能启动".into());
                    return fail(&app, &running, &generation, my_gen, mic_err);
                }
                // 停/存竞态保护：存 handle、running 检查、generation 检查必须在同一把
                // running 锁内完成（锁序 running → generation → handle_slot）。
                // stop_recording 和更新的 start_recording 都会递增 generation；
                // stop_recording 一律先置 running=false 再取 handle，且从不同时
                // 持有两把锁，因此无论 stop/新 start 发生在加载前、加载中还是加载
                // 后，与本线程的任意交错都是安全的：
                //  - stop 先到（running=false）：这里检测到 running==false，不存
                //    handle、不发 "recording"，直接把刚起好的会话原地停掉，避免
                //    孤儿会话。
                //  - 更快的 start #2 先到（running 仍为 true，但 generation 已被
                //    #2 抢先递增）：这里检测到 gen 不等于 my_gen，说明自己是过期
                //    加载（T1），同样不存 handle、不发 "recording"，原地停掉，让
                //    路给 #2 稍后存入的 handle——修复了"T1 的 handle 被 T2 覆盖
                //    而从未 stop()"的泄漏。
                //  - 都没发生：这里已把 handle 存进 handle_slot 并发出
                //    "recording"，stop_recording 随后正常取到该 handle 并停止。
                let running_guard = running.lock().unwrap();
                let gen_guard = generation.lock().unwrap();
                if !*running_guard || *gen_guard != my_gen {
                    drop(gen_guard);
                    drop(running_guard);
                    start.handle.stop();
                    return;
                }
                drop(gen_guard);
                let system_audio = classify_system(&start.active, &start.failed);
                *handle_slot.lock().unwrap() = Some(start.handle);
                drop(running_guard);
                let _ = app.emit(
                    "status",
                    ipc::StatusEvent { state: "recording".into(), system_audio },
                );
            }
            Err(e) => return fail(&app, &running, &generation, my_gen, format!("error: {e}")),
        }
    });

    Ok(())
}

#[tauri::command]
fn stop_recording(app: AppHandle, state: State<AppState>) {
    // 真停止协议：先置 running=false，再递增 generation（各自 statement-scoped
    // 锁，用完立即释放，从不同时持有两把），最后取 handle 并优雅停止（停
    // capture → flush 尾段 → 排干 finals → join）。递增 generation 让任何仍在
    // 加载窗口内的旧线程（无论其 running 检查读到 true 还是 false）都会因
    // generation 不匹配而放弃存 handle / 放弃清空 running，从而不会与本次
    // stop 产生孤儿会话或误清 running 的竞态。与 start_recording 加载线程的
    // 锁序一致（running → generation → handle_slot），且本函数从不同时持有
    // 两把锁，所以与加载线程的任意交错都不会死锁。
    { *state.running.lock().unwrap() = false; }
    { *state.generation.lock().unwrap() += 1; }
    let handle = state.handle.lock().unwrap().take();
    if let Some(h) = handle {
        h.stop();
    }
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
