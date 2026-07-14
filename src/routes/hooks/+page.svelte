<script lang="ts">
  import { HOOK_EVENTS } from "$lib/hooks.svelte";
</script>

<div class="page">
  <header class="topbar"><h1>钩子</h1></header>

  <p class="intro">
    笔记状态变化时自动执行你的命令或调用接口:停止录制后归档、精修完成后发通知——
    左侧新建一条钩子,选事件、填命令,配置完可以立即测试。
  </p>

  <section>
    <h2 class="section-title">可用事件</h2>
    <div class="rows">
      {#each HOOK_EVENTS as e (e.value)}
        <div class="row">
          <div class="row-info">
            <span class="row-label">{e.label}</span>
            <span class="row-desc"><code>{e.value}</code></span>
          </div>
        </div>
      {/each}
    </div>
  </section>

  <section>
    <h2 class="section-title">Shell 命令收到的环境变量</h2>
    <div class="rows">
      <div class="row">
        <div class="row-info">
          <span class="row-label"><code>VN_EVENT</code></span>
          <span class="row-desc">事件名,如 recording_stopped</span>
        </div>
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label"><code>VN_NOTE_ID</code></span>
          <span class="row-desc">笔记 id</span>
        </div>
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label"><code>VN_NOTE_TITLE</code></span>
          <span class="row-desc">笔记标题(取不到时为空)</span>
        </div>
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label"><code>VN_NOTE_TEXT</code></span>
          <span class="row-desc">笔记全文 markdown,精修稿优先——仅钩子勾选「附带笔记内容」时注入,下同</span>
        </div>
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label"><code>VN_NOTE_STARTED_AT</code> / <code>VN_NOTE_ENDED_AT</code></span>
          <span class="row-desc">开始/结束时间(RFC3339),未结束时结束为空</span>
        </div>
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label"><code>VN_NOTE_DURATION_SECS</code></span>
          <span class="row-desc">时长秒数</span>
        </div>
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label"><code>VN_NOTE_SPEAKERS</code></span>
          <span class="row-desc">说话人名单,顿号分隔</span>
        </div>
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label"><code>VN_NOTE_TEXT_TRUNCATED</code></span>
          <span class="row-desc">全文超 200KB 被截断时为 1,未截断不注入</span>
        </div>
      </div>
    </div>
  </section>

  <section>
    <h2 class="section-title">Webhook 收到的 JSON</h2>
    <pre class="snippet">{`POST <你的 URL>
content-type: application/json

{
  "event": "recording_stopped",
  "note_id": "…",
  "note_title": "…",
  "occurred_at": "2026-07-14T10:00:00+08:00",
  "note": {
    "started_at": "…", "ended_at": "…", "duration_secs": 3600,
    "speakers": ["张三"], "text": "…markdown…", "text_truncated": false
  }
}`}</pre>
    <p class="hint">note 字段仅在钩子勾选「附带笔记内容」时出现;停止录制时通常是原始稿,想要精修全文请挂「精修完成」事件。</p>
  </section>
</div>

<style>
  .page { padding: 0 1.5rem 2rem; }
  .topbar { position: sticky; top: 0; background: var(--canvas); padding: 1.1rem 0 0.6rem; }
  h1 { font-size: 1.15rem; font-weight: 500; margin: 0; }
  .intro {
    color: var(--ink-secondary);
    font-size: 0.9rem;
    line-height: 1.6;
    max-width: 42rem;
  }
  section { margin-top: 1.3rem; }
  .section-title {
    font-size: 0.82rem;
    font-weight: 500;
    color: var(--ink-secondary);
    margin: 0 0 0.45rem;
  }
  .rows {
    background: var(--surface);
    border-radius: var(--radius-lg);
    overflow: hidden;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 0.5rem 0.9rem;
    padding: 0.55rem 1rem;
    border-bottom: 1px solid var(--hairline);
  }
  .rows .row:last-child { border-bottom: none; }
  .row-info { flex: 1; display: flex; flex-direction: column; gap: 0.1rem; }
  .row-label { font-size: 0.92rem; color: var(--ink); }
  .row-desc { font-size: 0.8rem; color: var(--ink-secondary); line-height: 1.4; }
  code {
    font-size: 0.85em;
    background: var(--surface-press);
    border-radius: var(--radius-sm, 4px);
    padding: 0.05em 0.35em;
  }
  .snippet {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 0.8rem 1rem;
    font-size: 0.8rem;
    line-height: 1.55;
    color: var(--ink-secondary);
    overflow-x: auto;
    margin: 0;
  }
  .hint {
    color: var(--ink-faint);
    font-size: 0.8rem;
    margin: 0.5rem 0 0;
  }
</style>
