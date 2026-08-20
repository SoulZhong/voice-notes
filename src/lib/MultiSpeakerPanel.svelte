<script lang="ts">
  /* 多人混杂处置面板(quarantine_only 全流程):
     ① 选择要标记的原始说话人(修订稿入口会映射出多个候选 S)→ 打标(隔离即刻生效)
     ② 影响面 + 样本试听勾删(诚实呈现:历史样本无法归因,默认全不勾、不排序不高亮)
     ③ 质心残留二选一(接受残留 / 退回样本基线)→ 解除隔离,完成。
     阶段由后端 split_ops 落盘,面板可从任意中断点恢复(op.phase 驱动)。
     设计:docs/superpowers/specs/2026-08-20-mixed-speaker-split-design.md */
  import { convertFileSrc } from "@tauri-apps/api/core";
  import { t } from "$lib/i18n/index.svelte";
  import { speakerLabel } from "$lib/notes";
  import {
    cancelSplit,
    commitSplit,
    confirmMultiSamples,
    markSpeakerMulti,
    multiImpact,
    resolveMultiResidual,
    suggestSplitGroups,
    type MultiImpactReport,
    type SplitGroupIn,
    type SplitOp,
    type SplitSuggestOut,
  } from "$lib/multiSpeaker";
  import { createAudition, type PlayerLike } from "$lib/tidyAudio";

  let {
    noteId,
    speakers,
    candidateSpeakers,
    existingOp = null,
    segments = [],
    people = [],
    onAuditionSeg,
    onBeforeCommit,
    onClose,
    onChanged,
  }: {
    noteId: string;
    /** 原始稿说话人表(展示名用)。 */
    speakers: Record<string, { name?: string; person_id?: string | null }>;
    /** 打标候选(原始 S 编号;修订稿入口传该 R 涉及的全部 S,原始稿入口传单个)。 */
    candidateSpeakers: string[];
    /** 恢复入口:已存在的未完成 op。 */
    existingOp?: SplitOp | null;
    /** 原始段(拆分表格用:文本片段/时长/试听定位)。 */
    segments?: { seq: number; text: string; start_ms: number; end_ms: number }[];
    /** 会议搭子(拆分去向「库人物」下拉)。 */
    people?: { id: string; name: string }[];
    /** 试听某段(页面用主播放器实现:独奏所在轨 + seek + 段尾停)。 */
    onAuditionSeg?: (seq: number) => void;
    /** 提交拆分前排空编辑器未保存修改(后端会推进 refined revision,不排空会把
        正在编辑的文字冲掉——codex 实现轮一 P1⑫)。 */
    onBeforeCommit?: () => void | Promise<void>;
    onClose: () => void;
    /** 任何落盘变化后回调(页面刷新笔记/人物)。 */
    onChanged: () => void;
  } = $props();

  // svelte-ignore state_referenced_locally -- 面板一次性挂载:existingOp 只做初值(恢复入口)
  let op = $state<SplitOp | null>(existingOp);
  let impact = $state<MultiImpactReport | null>(null);
  // svelte-ignore state_referenced_locally -- 候选清单在面板打开那一刻定死,不随外部变
  let checked = $state<Record<string, boolean>>(
    Object.fromEntries(candidateSpeakers.map((s) => [s, true])),
  );
  let extraDelete = $state<Record<string, boolean>>({});
  let confirmSeen = $state(false);
  let busy = $state(false);
  let error = $state("");
  let done = $state(false);

  /** 面板所处的步骤:由 op.phase 推导,可从中断点恢复。 */
  const step = $derived.by(() => {
    if (done) return "done";
    if (!op) return "pick";
    if (op.phase === "plan") return "pick"; // 上次打标卡在半程:重试(后端复用同一 op)
    if (op.phase === "marked") return "samples";
    if (op.phase === "samples_handled") return "residual";
    if (op.phase === "residual_decided")
      return op.mode === "split_commit" ? "split" : "residual";
    if (["reserved", "segments_reassigned", "reenrolled"].includes(op.phase)) return "split_resume";
    if (op.phase === "cancel_requested") return "cancelling";
    if (op.phase === "released") return "finishing";
    return "samples";
  });

  async function refreshImpact() {
    if (!op) return;
    try {
      impact = await multiImpact(op.op_id);
    } catch (e) {
      error = `${e}`;
    }
  }
  $effect(() => {
    if (op && !impact) void refreshImpact();
  });

  async function doMark() {
    const ids = candidateSpeakers.filter((s) => checked[s]);
    if (ids.length === 0 || busy) return;
    busy = true;
    error = "";
    try {
      const opId = await markSpeakerMulti(noteId, ids);
      op = {
        op_id: opId,
        mode: "quarantine_only",
        note_id: noteId,
        speaker_ids: ids,
        affected_persons: [],
        phase: "marked",
        samples_confirm_seen: false,
        created_at: "",
        updated_at: "",
      };
      onChanged();
      await refreshImpact();
    } catch (e) {
      error = t("speakers.multi.failed", { e: `${e}` });
    }
    busy = false;
  }

  async function doSamples() {
    if (!op || busy) return;
    busy = true;
    error = "";
    try {
      const extras = Object.entries(extraDelete)
        .filter(([, v]) => v)
        .map(([k]) => k);
      await confirmMultiSamples(op.op_id, extras, confirmSeen);
      op = { ...op, phase: "samples_handled" };
      onChanged();
      await refreshImpact();
    } catch (e) {
      error = `${e}`;
    }
    busy = false;
  }

  let thenSplit = $state(false);
  async function doResidual(choice: "accept" | "baseline") {
    if (!op || busy) return;
    busy = true;
    error = "";
    try {
      await resolveMultiResidual(op.op_id, choice, thenSplit);
      if (thenSplit) {
        op = { ...op, phase: "residual_decided", mode: "split_commit" };
        void loadSplitSuggestion();
      } else {
        done = true;
      }
      onChanged();
    } catch (e) {
      error = `${e}`;
    }
    busy = false;
  }

  // ── 拆分:建议分组 + 每段一个「归入哪组」下拉 + 每组去向 ──
  type Dest = { kind: SplitGroupIn["dest_kind"]; id: string | null };
  let splitSug = $state<SplitSuggestOut | null>(null);
  let splitLoading = $state(false);
  /** seq → 组键("g0".."gN" / "u"=无法判定桶)。 */
  let segGroup = $state<Record<number, string>>({});
  let dests = $state<Record<string, Dest>>({});
  let enrollNotes = $state("");

  const groupKeys = $derived.by(() => {
    const keys = new Set(Object.values(segGroup));
    return [...keys].sort();
  });
  const segByseq = $derived(new Map(segments.map((s) => [s.seq, s])));
  const fmtDur = (ms: number) => `${Math.round(ms / 1000)}s`;
  const groupDur = (key: string) =>
    Object.entries(segGroup)
      .filter(([, g]) => g === key)
      .reduce((acc, [q]) => {
        const s = segByseq.get(Number(q));
        return acc + (s ? s.end_ms - s.start_ms : 0);
      }, 0);

  async function loadSplitSuggestion() {
    if (!op) return;
    splitLoading = true;
    error = "";
    try {
      const sug = await suggestSplitGroups(op.op_id);
      splitSug = sug;
      const map: Record<number, string> = {};
      const d: Record<string, Dest> = {};
      sug.groups.forEach((g, i) => {
        for (const q of g.seqs) map[q] = `g${i}`;
        d[`g${i}`] = { kind: "new_speaker", id: null }; // 建议只展示,不自动应用
      });
      for (const q of sug.undetermined) map[q] = "u";
      if (sug.undetermined.length > 0) d["u"] = { kind: "keep", id: null };
      segGroup = map;
      dests = d;
    } catch (e) {
      error = `${e}`;
    }
    splitLoading = false;
  }

  async function doCommitSplit(resume: boolean) {
    if (!op || busy) return;
    busy = true;
    error = "";
    try {
      await onBeforeCommit?.();
      const groups: SplitGroupIn[] = resume
        ? []
        : groupKeys.map((key) => ({
            seqs: Object.entries(segGroup)
              .filter(([, g]) => g === key)
              .map(([q]) => Number(q))
              .sort((a, b) => a - b),
            dest_kind: dests[key]?.kind ?? "keep",
            dest_id: dests[key]?.id ?? null,
          }));
      enrollNotes = await commitSplit(op.op_id, groups);
      done = true;
      onChanged();
    } catch (e) {
      error = `${e}`;
    }
    busy = false;
  }

  async function doCancelSplit() {
    if (!op || busy) return;
    busy = true;
    error = "";
    try {
      await cancelSplit(op.op_id);
      done = true;
      onChanged();
    } catch (e) {
      error = `${e}`;
    }
    busy = false;
  }
  $effect(() => {
    if (step === "split" && !splitSug && !splitLoading) void loadSplitSuggestion();
  });

  // 样本试听:单实例互斥,复用 tidyAudio 控制器(样本是本机文件,convertFileSrc 可播)。
  let playingKey = $state<string | null>(null);
  const audition = createAudition(
    (src) => new Audio(convertFileSrc(src)) as unknown as PlayerLike,
    (k) => (playingKey = k),
    (msg) => (error = t("speakers.auditionFailed", { msg })),
  );
  $effect(() => () => audition.stop());

  const sampleName = (path: string) => path.split("/").pop() ?? path;
</script>

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div class="overlay" role="presentation" onclick={(e) => { if (e.target === e.currentTarget) onClose(); }}>
  <div class="panel">
    <div class="head">
      <span class="title">{t("speakers.multi.title")}</span>
      <button class="link" onclick={onClose}>{t("speakers.cancel")}</button>
    </div>
    {#if error}<div class="banner">{error}</div>{/if}

    {#if step === "pick"}
      <p class="note">{t("speakers.multi.mapNotice")}</p>
      {#each candidateSpeakers as sid (sid)}
        <label class="row-check">
          <input type="checkbox" bind:checked={checked[sid]} />
          {speakerLabel(sid, "mic", speakers)}
        </label>
      {/each}
      <div class="actions">
        <button class="primary" disabled={busy || candidateSpeakers.every((s) => !checked[s])} onclick={doMark}>
          {t("speakers.chipMarkMulti")}
        </button>
      </div>
    {:else if step === "samples"}
      <div class="banner ok">{t("speakers.multi.marked")}</div>
      {#if impact}
        {#each impact.persons as p (p.person_id)}
          <section class="person">
            <div class="p-title">{t("speakers.multi.impactPerson", { name: p.name || p.person_id })}</div>
            <div class="note">{t("speakers.multi.countShare", { est: p.cluster_count_est, total: p.person_count_total })}</div>
            {#if p.has_session_centroid}<div class="note warn">{t("speakers.multi.sessionCentroid")}</div>{/if}
            <div class="note">{t("speakers.multi.metaResidual")}</div>
            {#if p.samples.length > 0}
              <div class="note strong">{t("speakers.multi.samplesHeader")}</div>
              {#each p.samples as sp (sp.path)}
                <div class="sample-row">
                  <button class="link" onclick={() => audition.toggle(sp.path, sp.audition_path)}>
                    {playingKey === sp.path ? t("speakers.stop") : t("speakers.playSample")}
                  </button>
                  <span class="mono">{sampleName(sp.path)}</span>
                  {#if sp.from_marked_cluster}
                    <span class="tag auto">{t("speakers.multi.fromThisNote")}</span>
                  {:else}
                    <span class="tag">{t("speakers.multi.unknownOrigin")}</span>
                    <label class="row-check inline">
                      <input type="checkbox" bind:checked={extraDelete[sp.path]} />
                      {t("speakers.delete")}
                    </label>
                  {/if}
                </div>
              {/each}
            {/if}
          </section>
        {/each}
        <label class="row-check">
          <input type="checkbox" bind:checked={confirmSeen} />
          {t("speakers.multi.confirmSeen")}
        </label>
        <div class="actions">
          <button class="primary" disabled={busy || !confirmSeen} onclick={doSamples}>
            {t("speakers.multi.deleteChecked")}
          </button>
        </div>
      {/if}
    {:else if step === "residual"}
      <div class="p-title">{t("speakers.multi.residualTitle")}</div>
      <label class="row-check">
        <input type="checkbox" bind:checked={thenSplit} />
        {t("speakers.split.enter")}
      </label>
      <div class="choice">
        <button class="option" disabled={busy} onclick={() => doResidual("accept")}>
          <span class="opt-name">{t("speakers.multi.residualAccept")}</span>
          <span class="opt-desc">{t("speakers.multi.residualAcceptDesc")}</span>
        </button>
        <button class="option danger" disabled={busy} onclick={() => doResidual("baseline")}>
          <span class="opt-name">{t("speakers.multi.residualBaseline")}</span>
          <span class="opt-desc">{t("speakers.multi.residualBaselineDesc")}</span>
        </button>
      </div>
    {:else if step === "split"}
      <div class="p-title">{t("speakers.split.title")}</div>
      {#if splitLoading || !splitSug}
        <div class="note">{t("speakers.split.loading")}</div>
      {:else}
        <div class="note strong">{t("speakers.split.suggestN", { n: splitSug.groups.length })}</div>
        <table class="seg-table">
          <thead>
            <tr><th>{t("speakers.split.segCol")}</th><th></th><th>{t("speakers.split.groupCol")}</th></tr>
          </thead>
          <tbody>
            {#each Object.keys(segGroup).map(Number).sort((a, b) => a - b) as q (q)}
              {@const sg = segByseq.get(q)}
              <tr>
                <td class="seg-text">
                  <span class="mono">#{q}</span>
                  {#if sg}<span class="dur">{fmtDur(sg.end_ms - sg.start_ms)}</span> {sg.text.slice(0, 40)}{/if}
                </td>
                <td>
                  {#if onAuditionSeg}
                    <button class="link" onclick={() => onAuditionSeg(q)}>{t("speakers.playSample")}</button>
                  {/if}
                </td>
                <td>
                  <select
                    value={segGroup[q]}
                    onchange={(e) => {
                      const v = e.currentTarget.value;
                      if (v === "__new") {
                        // 补组:分配一个新的组键并给它一个去向条目(默认新说话人)。
                        let n = 0;
                        while (groupKeys.includes(`g${n}`) || dests[`g${n}`]) n++;
                        const key = `g${n}`;
                        dests[key] = { kind: "new_speaker", id: null };
                        segGroup[q] = key;
                      } else {
                        segGroup[q] = v;
                      }
                    }}
                  >
                    {#each groupKeys as key (key)}
                      <option value={key}>
                        {key === "u" ? t("speakers.split.undetermined") : t("speakers.split.groupN", { n: key.slice(1) })}
                      </option>
                    {/each}
                    <option value="__new">{t("speakers.split.destNew")}</option>
                  </select>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
        {#each groupKeys as key (key)}
          {@const gi = key === "u" ? -1 : Number(key.slice(1))}
          <div class="group-row">
            <span class="g-name">
              {key === "u" ? t("speakers.split.undetermined") : t("speakers.split.destLabel", { n: key.slice(1), c: Object.values(segGroup).filter((g) => g === key).length, dur: fmtDur(groupDur(key)) })}
            </span>
            {#if gi >= 0 && splitSug.groups[gi]?.suggested}
              {@const sug = splitSug.groups[gi].suggested}
              <span class="tag">{t("speakers.split.suggestedTag", { name: sug[1] || sug[0], sim: sug[2].toFixed(2) })}</span>
            {/if}
            {#if dests[key]}
              <select
                value={`${dests[key].kind}:${dests[key].id ?? ""}`}
                onchange={(e) => {
                  const [kind, id] = e.currentTarget.value.split(":");
                  dests[key] = { kind: kind as Dest["kind"], id: id || null };
                }}
              >
                <option value="keep:">{t("speakers.split.destKeep")}</option>
                <option value="new_speaker:">{t("speakers.split.destNew")}</option>
                {#each Object.entries(speakers) as [sid, m] (sid)}
                  <option value={`existing_speaker:${sid}`}>{t("speakers.split.destExisting", { name: m.name || sid })}</option>
                {/each}
                {#each people as pp (pp.id)}
                  <option value={`person:${pp.id}`}>{t("speakers.split.destPerson", { name: pp.name || pp.id })}</option>
                {/each}
              </select>
            {/if}
          </div>
        {/each}
        <div class="actions">
          <button class="link" disabled={busy} onclick={doCancelSplit}>{t("speakers.split.cancel")}</button>
          <button class="primary" disabled={busy} onclick={() => doCommitSplit(false)}>{t("speakers.split.commit")}</button>
        </div>
      {/if}
    {:else if step === "cancelling"}
      <div class="note">{t("speakers.split.cancel")}</div>
      <div class="actions">
        <button class="primary" disabled={busy} onclick={doCancelSplit}>{t("speakers.split.cancel")}</button>
      </div>
    {:else if step === "finishing"}
      <div class="actions">
        <button
          class="primary"
          disabled={busy}
          onclick={() => doResidual((op?.residual_choice as "accept" | "baseline") ?? "accept")}
        >
          {t("speakers.ok")}
        </button>
      </div>
    {:else if step === "split_resume"}
      <div class="note">{t("speakers.multi.resume")}</div>
      <div class="actions">
        <button class="primary" disabled={busy} onclick={() => doCommitSplit(true)}>{t("speakers.split.continueCommit")}</button>
      </div>
    {:else}
      <div class="banner ok">
        {enrollNotes ? t("speakers.split.enrollNotes", { notes: enrollNotes }) : t("speakers.multi.doneToast")}
      </div>
      <div class="actions"><button class="primary" onclick={onClose}>{t("speakers.ok")}</button></div>
    {/if}
  </div>
</div>

<style>
  .seg-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 12.5px;
    margin: 8px 0;
  }
  .seg-table th {
    text-align: left;
    font-weight: 500;
    opacity: 0.6;
    padding: 2px 4px;
  }
  .seg-table td {
    padding: 3px 4px;
    border-top: 1px solid rgba(128, 128, 128, 0.18);
  }
  .seg-text .dur {
    opacity: 0.6;
    margin: 0 6px;
  }
  .group-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 0;
    font-size: 12.5px;
    flex-wrap: wrap;
  }
  .g-name {
    font-weight: 600;
  }
  select {
    background: inherit;
    color: inherit;
    border: 1px solid rgba(128, 128, 128, 0.45);
    border-radius: 6px;
    padding: 2px 6px;
    font-size: 12px;
    max-width: 220px;
  }

  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.45);
    z-index: 60;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .panel {
    background: var(--bg, #1b1b1f);
    color: var(--fg, #eee);
    border: 1px solid rgba(128, 128, 128, 0.35);
    border-radius: 12px;
    padding: 16px 18px;
    width: min(560px, 92vw);
    max-height: 84vh;
    overflow-y: auto;
  }
  .head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 8px;
  }
  .title {
    font-weight: 600;
  }
  .banner {
    border: 1px solid rgba(220, 120, 120, 0.5);
    border-radius: 8px;
    padding: 6px 10px;
    margin: 6px 0;
    font-size: 13px;
  }
  .banner.ok {
    border-color: rgba(120, 200, 140, 0.5);
  }
  .note {
    font-size: 12.5px;
    opacity: 0.85;
    margin: 4px 0;
  }
  .note.warn {
    opacity: 1;
    color: #e8c46a;
  }
  .note.strong {
    opacity: 1;
    font-weight: 600;
    margin-top: 8px;
  }
  .person {
    border-top: 1px solid rgba(128, 128, 128, 0.25);
    padding: 8px 0;
    margin-top: 8px;
  }
  .p-title {
    font-weight: 600;
    margin: 6px 0;
  }
  .sample-row {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 12.5px;
    padding: 2px 0;
  }
  .mono {
    font-family: ui-monospace, monospace;
    opacity: 0.8;
  }
  .tag {
    font-size: 11px;
    border: 1px solid rgba(128, 128, 128, 0.5);
    border-radius: 6px;
    padding: 0 6px;
    opacity: 0.85;
  }
  .tag.auto {
    border-color: rgba(120, 200, 140, 0.6);
  }
  .row-check {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    margin: 6px 0;
  }
  .row-check.inline {
    display: inline-flex;
    margin: 0;
  }
  .actions {
    margin-top: 12px;
    display: flex;
    justify-content: flex-end;
  }
  .primary {
    padding: 6px 14px;
    border-radius: 8px;
    border: 1px solid rgba(128, 128, 128, 0.5);
    background: rgba(120, 160, 255, 0.18);
    cursor: pointer;
  }
  .primary:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .link {
    background: none;
    border: none;
    color: inherit;
    text-decoration: underline;
    cursor: pointer;
    font-size: 12.5px;
    padding: 0;
  }
  .choice {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-top: 8px;
  }
  .option {
    text-align: left;
    border: 1px solid rgba(128, 128, 128, 0.45);
    border-radius: 10px;
    padding: 10px 12px;
    background: none;
    color: inherit;
    cursor: pointer;
  }
  .option.danger {
    border-color: rgba(220, 120, 120, 0.55);
  }
  .opt-name {
    display: block;
    font-weight: 600;
    margin-bottom: 4px;
  }
  .opt-desc {
    display: block;
    font-size: 12px;
    opacity: 0.8;
    line-height: 1.5;
  }
</style>
