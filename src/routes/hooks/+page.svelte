<div class="page">
  <header class="topbar"><h1>钩子</h1></header>

  <p class="intro">
    笔记状态变化时自动执行你的命令或调用接口：停止录制后归档、Aing 结束后发通知——
    左侧新建一条钩子,选事件、填命令,配置完可以立即测试。
  </p>

  <section>
    <h2 class="section-title">可用事件</h2>
    <div class="flow-card">
      <svg class="flow-svg diagram" viewBox="0 0 820 286" role="img" aria-label="笔记生命周期状态图：录制中停止后进入录制完成，执行 Aing 后按结果进入 AI 已完成或 AI 失败状态">
        <defs>
          <marker id="hk-arrow" viewBox="0 0 10 10" refX="8.5" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
            <path d="M0,0 L10,5 L0,10 z" class="arrowhead" />
          </marker>
        </defs>

        <!-- 转移边(先画,压在节点下) -->
        <!-- 起点 → 录制中 -->
        <line class="edge" x1="36" y1="92" x2="77" y2="92" marker-end="url(#hk-arrow)" />
        <!-- 录制中 → 录制完成 -->
        <line class="edge" x1="203" y1="92" x2="282" y2="92" marker-end="url(#hk-arrow)" />
        <!-- 录制完成 → Aing 动作 → 成功/失败结果 -->
        <line class="edge" x1="408" y1="92" x2="547" y2="92" marker-end="url(#hk-arrow)" />
        <path class="decision" d="M550 92 570 72 590 92 570 112Z" />
        <line class="edge" x1="590" y1="86" x2="682" y2="65" marker-end="url(#hk-arrow)" />
        <line class="edge" x1="590" y1="98" x2="682" y2="170" marker-end="url(#hk-arrow)" />
        <!-- 录制中 → 已暂停 -->
        <line class="edge" x1="125" y1="121" x2="125" y2="202" marker-end="url(#hk-arrow)" />
        <!-- 已暂停 → 录制中 -->
        <line class="edge" x1="155" y1="202" x2="155" y2="123" marker-end="url(#hk-arrow)" />

        <!-- 起止标记 -->
        <circle class="start-dot" cx="30" cy="92" r="5" />

        <!-- 状态节点 -->
        <g class="node">
          <rect class="node-box" x="80" y="63" width="120" height="58" rx="10" />
          <text class="node-label" x="140" y="92" text-anchor="middle" dominant-baseline="central">录制中</text>
        </g>
        <g class="node">
          <rect class="node-box" x="285" y="63" width="120" height="58" rx="10" />
          <text class="node-label" x="345" y="92" text-anchor="middle" dominant-baseline="central">录制完成</text>
        </g>
        <g class="node">
          <rect class="node-box" x="685" y="36" width="120" height="58" rx="10" />
          <text class="node-label" x="745" y="65" text-anchor="middle" dominant-baseline="central">
            <tspan>AI</tspan><tspan class="state-check" dx="6">✓</tspan>
          </text>
        </g>
        <g class="node">
          <rect class="node-box" x="685" y="141" width="120" height="58" rx="10" />
          <text class="node-label" x="745" y="170" text-anchor="middle" dominant-baseline="central">
            <tspan>AI</tspan><tspan class="state-failed" dx="6">×</tspan>
          </text>
        </g>
        <g class="node">
          <rect class="node-box" x="80" y="205" width="120" height="58" rx="10" />
          <text class="node-label" x="140" y="234" text-anchor="middle" dominant-baseline="central">已暂停</text>
        </g>

        <!-- 边标签(中文名 + 事件键) -->
        <text class="edge-label" text-anchor="middle" x="52" y="42">
          <tspan class="cn">录制开始</tspan>
          <tspan class="key" x="52" dy="15">recording_started</tspan>
        </text>
        <text class="edge-label" text-anchor="middle" x="242" y="42">
          <tspan class="cn">录制停止</tspan>
          <tspan class="key" x="242" dy="15">recording_stopped</tspan>
        </text>
        <text class="action-label" text-anchor="middle" x="477" y="78">
          <tspan class="action-name">Aing</tspan>
        </text>
        <text class="outcome-label" text-anchor="middle" x="636" y="61">成功</text>
        <text class="outcome-label" text-anchor="middle" x="636" y="141">失败</text>
        <text class="edge-label" text-anchor="end" x="116" y="151">
          <tspan class="cn">录制暂停</tspan>
          <tspan class="key" x="116" dy="15">recording_paused</tspan>
        </text>
        <text class="edge-label" text-anchor="start" x="165" y="184">
          <tspan class="cn">录制恢复</tspan>
          <tspan class="key" x="165" dy="15">recording_resumed</tspan>
        </text>
      </svg>
      <p class="flow-caption">
        方框表示状态，连线表示动作。录制完成后执行 Aing；成功后标记为「AI ✓」，失败后标记为「AI ×」并可重新执行。
        钩子可在 Aing 开始（refine_started）或结束（refine_finished）时触发。
      </p>
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
          <span class="row-desc">笔记全文 markdown,修订稿优先——仅钩子勾选「附带笔记内容」时注入,下同</span>
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
    <p class="hint">note 字段仅在钩子勾选「附带笔记内容」时出现；停止录制时通常是原始稿，想要 AI 整理后的全文请挂「Aing 结束」事件。</p>
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
  /* 生命周期状态图:节点=状态,边=事件转移,与后端 hook_events() 映射一致 */
  .flow-card {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 1rem 0.75rem 0.75rem;
  }
  .flow-svg {
    width: 100%;
    height: auto;
    display: block;
    max-width: 780px;
    margin: 0 auto;
  }
  .diagram text { font-family: inherit; }
  .diagram .node-box {
    fill: var(--surface-press);
    stroke: var(--hairline);
    stroke-width: 1;
  }
  .diagram .node-label {
    fill: var(--ink);
    font-size: 14px;
    font-weight: 500;
  }
  .diagram .state-check { fill: var(--success); font-weight: 700; }
  .diagram .state-failed { fill: var(--danger); font-weight: 700; }
  .diagram .edge {
    stroke: var(--ink-faint);
    stroke-width: 1.5;
    fill: none;
  }
  .diagram .action-label {
    fill: var(--ink-secondary);
    font-size: 12.5px;
    font-weight: 500;
  }
  .diagram .action-name { font-size: 12.5px; }
  .diagram .decision { fill: var(--surface-press); stroke: var(--hairline); stroke-width: 1; }
  .diagram .outcome-label { fill: var(--ink-faint); font-size: 11px; }
  .diagram .arrowhead { fill: var(--ink-faint); }
  .diagram .start-dot { fill: var(--ink-secondary); }
  .diagram .cn { fill: var(--ink-secondary); font-size: 12.5px; }
  .diagram .key {
    fill: var(--ink-faint);
    font-size: 10.5px;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }
  .flow-caption {
    color: var(--ink-faint);
    font-size: 0.8rem;
    line-height: 1.5;
    margin: 0.7rem 0.25rem 0;
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
