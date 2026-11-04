// e2e 用的 Tauri 后端模拟(注入到页面,先于应用脚本执行)。
// 照 @tauri-apps/api/mocks 的 mockIPC 实现 __TAURI_INTERNALS__,并带事件插件:
// - 前端 invoke 的每一次调用都记进 window.__e2e.calls,供断言;
// - window.__e2e.emit(event, payload) 从「后端」推事件给前端;
// - 固定数据在 FIXTURES;没有写到的命令返回 null 并记进 window.__e2e.unmocked。
// 这是浏览器脚本(被 run.mjs 读成字符串注入),不要用 import。
(() => {
  const params = window.__E2E_PARAMS__ || {};
  const now = "2026-11-04T10:00:00+08:00";
  const NOTE_ID = "20261104-100000";

  const settings = {
    data_dir: null, models_dir: null, asr_model: "sense_voice", asr_provider: "", asr_hotwords: "",
    speaker_match: "nearest", asr_mode: "local", cloud_asr_provider: "volcano", volc_app_key: "",
    volc_access_key: "", dashscope_api_key: "", speaker_model: "campplus",
    theme: params.mode || "dark", theme_family: params.family || "classic", ui_lang: "zh",
    capture_path: "aec", language_filter: true, auto_input_pick: true, audio_retention: "forever",
    shortcut_enabled: false, shortcut: "Alt+CmdOrCtrl+R", memo_shortcut_enabled: true,
    memo_shortcut: "Alt+CmdOrCtrl+M", tray_enabled: true, telemetry_enabled: false, refine_enabled: false,
    llm_profiles: [], agent_profiles: [], refine_executor: "", relations_executor: "", onboarded: true,
    completed_guides: ["ai-tools-v1", "recording-entry-v1", "recording-basics-v1", "note-reading-v1", "people-library-v1", "knowledge-graph-v1", "hooks-automation-v1", "settings-basics-v1"], mcp_allow_control: false, mcp_onboarded: true, audio_scheme: "b",
    calendar_match_enabled: false, identify_auto_apply: false, device_enabled: false, device_name: null,
    show_all_settings: true, dictation_engine: "apple", dictation_save_text: true, dictation_save_audio: false,
    dictation_ai_access: false, scene_auto_fold: true,
  };

  const segments = [
    { seq: 0, source: "mic", text: "先看一下上线时间。", start_ms: 0, end_ms: 4000, speaker: "S1" },
    { seq: 1, source: "system", text: "我们定在下周二上线,周一封版。", start_ms: 5000, end_ms: 9000, speaker: "S2" },
    { seq: 2, source: "mic", text: "好,那就这么定。", start_ms: 12000, end_ms: 14000, speaker: "S1" },
  ];
  // 备注 = 一篇 Markdown(行尾 <!-- mm:ss --> 是时间点);memos 是行视图(同后端 parse_lines 的简化版)。
  let memoText = "定了:周二上线 <!-- 00:06 -->\n";
  const revOf = (t) => (t ? `r${t.length}_${[...t].reduce((h, c) => (h * 31 + c.charCodeAt(0)) >>> 0, 7)}` : "0");
  const clock = (ms) => { const s = Math.floor(ms / 1000); return `${String(Math.floor(s / 60)).padStart(2, "0")}:${String(s % 60).padStart(2, "0")}`; };
  const parseLines = (t) => t.split("\n").flatMap((l, i) => {
    const m = /^(.*?)\s*<!--\s*(\d+):(\d\d)\s*-->\s*$/.exec(l);
    if (!m) return [];
    const md = m[1];
    const text = md.replace(/^(#+\s+|[-*+]\s+(\[[ xX]\]\s+)?|\d+\.\s+|>\s*)/, "").trim();
    return [{ id: `L${i + 1}`, at_ms: (Number(m[2]) * 60 + Number(m[3])) * 1000, text: text === "📍" ? "" : text, md, line: i + 1 }];
  });
  const memoDocNow = () => ({ text: memoText, rev: revOf(memoText) });
  const emitMemos = (noteId) => setTimeout(() => window.__e2e.emit("memos_changed", { note_id: noteId, memos: parseLines(memoText), doc: memoDocNow() }), 0);
  let memos = parseLines(memoText);
  const note = {
    meta: { schema_version: 1, id: NOTE_ID, title: "周会", started_at: now, ended_at: "2026-11-04T10:30:00+08:00", state: "complete", attendees: [], attendees_removed: [] },
    segments, suppressed_segments: [], skipped_lines: 0,
    speakers: { S1: { name: "张三", sources: ["mic"] }, S2: { name: "李四", sources: ["system"] } },
  };
  const recordingLive = params.recording === true;

  const FIXTURES = {
    get_settings: () => settings,
    set_settings: (a) => { Object.assign(settings, a.settings || a.s || {}); return null; },
    list_notes: () => [{ id: NOTE_ID, title: "周会", started_at: now, duration_secs: 1800, state: recordingLive ? "active" : "complete" }],
    list_dictation_notes: () => [],
    get_note: () => note,
    note_memos: () => parseLines(memoText),
    memo_doc: () => memoDocNow(),
    save_memo_doc: (a) => {
      if (a.baseRev !== revOf(memoText)) return { kind: "conflict", doc: memoDocNow() };
      memoText = a.text;
      memos = parseLines(memoText);
      emitMemos(a.noteId);
      return { kind: "saved", doc: memoDocNow() };
    },
    edit_memo_line: (a) => {
      const lines = memoText.split("\n");
      const hit = parseLines(memoText).reverse().find((m) => m.at_ms === a.atMs && m.text === a.oldText);
      const block = `${a.text} <!-- ${clock(a.atMs)} -->`;
      if (hit) lines[hit.line - 1] = block;
      else lines.push("", block);
      memoText = lines.join("\n").replace(/\n*$/, "\n");
      memos = parseLines(memoText);
      emitMemos(a.noteId);
      return null;
    },
    note_edits: () => ({ schema_version: 1, cuts: [] }),
    search_memos: (a) => parseLines(memoText).filter((m) => m.text.includes(a.query)).map((m) => ({ note_id: NOTE_ID, memo_id: m.id, at_ms: m.at_ms, text: m.text })),
    memo_anchor_now: () => 7000,
    recording_status: () =>
      recordingLive
        ? { state: "recording", system_audio: "on", note_id: NOTE_ID, diarization: "on", elapsed_ms: 15000, input_override: "" }
        : { state: "idle", system_audio: "", note_id: "", diarization: "", elapsed_ms: 0, input_override: "" },
    models_status: () => ({ recording_ready: true, diarization_ready: true, artifacts: [], root: "/models" }),
    apple_asr_status: () => "Ready",
    list_people: () => [],
    // 说话人条编辑(录制中走 writer 路径;后端随后推全量 speakers 事件,删除时带 cleared)
    assign_note_speaker_person: () => null,
    clear_note_speaker_person: () => null,
    person_add_email: () => null,
    delete_note_speaker: (a) => {
      delete note.speakers[a.speakerId];
      const speakers = Object.entries(note.speakers).map(([id, m]) => ({ id, name: m.name, sources: m.sources, person_id: m.person_id ?? null }));
      setTimeout(() => window.__e2e.emit("speakers", { speakers, merged: null, cleared: a.speakerId }), 0);
      return null;
    },
    list_speakers: () => [],
    tidy_inbox: () => [],
    pending_review: () => [],
    semantic_graph: () => ({ entities: [], relations: [], notes: [] }),
    graph_entities: () => [],
    graph_data: () => ({ nodes: [], edges: [] }),
    list_hooks: () => [],
    mcp_status: () => ({ registered: [], available: [] }),
    device_status: () => ({
      enabled: false, device_name: null, running: false, engine: "apple", missing_models: [], accessibility: true,
      speech_permission: "authorized", platform: "macos", sim: false,
      link: { state: null, text: "", ended: null, mismatch: null, firmware: null },
    }),
    output_is_bluetooth: () => false,
    note_tracks: () => [],
    refine_status: () => null,
    get_refined: () => null,
    check_update: () => ({ has_update: false, latest: "", current: "0.21.2", url: "" }),
    download_route_mirror: () => false,
    calendar_permission: () => "unavailable",
    audio_disk_usage: () => 0,
    voiceprint_library_model: () => "campplus",
    apply_confident_merges: () => ({ applied: [], remaining: [] }),
    list_merge_receipts: () => [],
    list_dismissed_tidy_items: () => [],
    list_identify_suggestions: () => [],
    telemetry_enabled: () => false,
    set_ui_visible: () => null,
    set_playback_active: () => null,
    ai_logs_query: () => ({ total: 0, entries: [] }),
    refine_agents_probe: () => ({ claude: null, codex: null, gemini: null, pi: null }),
    mcp_agents_status: () => [],
    mcp_skill_status: () => "absent",
    mcp_manual_snippet: () => "",
    mcp_healed_count: () => 0,
    mcp_capabilities: () => ({ tools: [], cli: [] }),
    list_split_ops: () => [],
    get_scene: () => null,
    latest_undoable_split: () => null,
    note_clip_ranks: () => ({}),
    note_audio_info: () => [],
    mixed_playback_info: () => ({ track: null, untrusted: null, seek_offset_ms: {}, ab_caveat: false }),
    note_related: () => [],
    note_entity_links: () => [],
    mixed_input_status: () => null,
    note_refining: () => false,
    retranscribe_status: () => null,
    mixed_regen_status: () => null,
    screen_capture_permission: () => true,
    input_volume: () => null,
    precheck_recording: () => [],
    memo_panel_state: () => ({ note_id: NOTE_ID, memo: parseLines(memoText)[0] }),
    memo_panel_hide: () => null,
    "plugin:autostart|is_enabled": () => false,
    "plugin:app|version": () => "0.21.2",
  };

  const calls = [];
  const unmocked = new Set();
  const listeners = new Map();
  const callbacks = new Map();
  let nextId = 1;
  function registerCallback(cb, once = false) {
    const id = nextId++;
    callbacks.set(id, (data) => {
      if (once) callbacks.delete(id);
      return cb && cb(data);
    });
    return id;
  }
  function emit(event, payload) {
    for (const h of listeners.get(event) || []) callbacks.get(h)?.({ event, id: h, payload });
  }
  async function invoke(cmd, args = {}) {
    calls.push({ cmd, args: JSON.parse(JSON.stringify(args ?? {})) });
    if (cmd === "plugin:event|listen") {
      if (!listeners.has(args.event)) listeners.set(args.event, []);
      listeners.get(args.event).push(args.handler);
      return args.handler;
    }
    if (cmd === "plugin:event|unlisten") {
      const l = listeners.get(args.event) || [];
      const i = l.indexOf(args.eventId);
      if (i >= 0) l.splice(i, 1);
      return null;
    }
    if (cmd === "plugin:event|emit") {
      emit(args.event, args.payload);
      return null;
    }
    const f = FIXTURES[cmd];
    if (!f) {
      unmocked.add(cmd);
      return null;
    }
    return f(args);
  }

  window.__TAURI_INTERNALS__ = {
    invoke,
    transformCallback: registerCallback,
    unregisterCallback: (id) => callbacks.delete(id),
    runCallback: (id, data) => callbacks.get(id)?.(data),
    callbacks,
    convertFileSrc: (p) => p,
    metadata: { currentWindow: { label: "main" }, currentWebview: { windowLabel: "main", label: "main" } },
  };
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: (_e, id) => callbacks.delete(id) };
  window.__e2e = { calls, unmocked, emit, NOTE_ID, get memos() { return memos; }, get memoText() { return memoText; } };
})();
