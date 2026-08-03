<script lang="ts">
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { ask } from "@tauri-apps/plugin-dialog";
  import { t } from "$lib/i18n/index.svelte";
  import {
    listHooks,
    saveHooks,
    testHook,
    newHook,
    hooks as hooksStore,
    HOOK_EVENTS,
    type HookCfg,
    type HookKind,
  } from "$lib/hooks.svelte";

  const routeId = $derived($page.params.id as string);
  const isNew = $derived(routeId === "new");

  let cfg = $state<HookCfg | null>(null);
  let loadError = $state("");
  let saveError = $state("");
  // 测试结果三态:null=没测过,ok/err 带信息;改配置即失效清空,防拿旧结果背书新命令
  let testResult = $state<{ ok: boolean; msg: string } | null>(null);
  let testing = $state(false);
  let saving = $state(false);

  // 路由 id 变化(含 new→保存后的真 id、侧栏点另一条)整页重载
  $effect(() => {
    const id = routeId;
    loadError = "";
    testResult = null;
    if (id === "new") {
      cfg = newHook();
      return;
    }
    // 先清掉上一条的数据,避免加载窗口期短暂展示旧钩子内容
    cfg = null;
    listHooks()
      .then((list) => {
        // 过期守卫:快速切换 A→B 时,A 的响应可能晚于 B 返回,若不校验会把 B 页
        // 的表单覆盖成 A 的数据(此后保存还会静默写回 A)。闭包 id ≠ 当前 routeId 即丢弃。
        if (id !== routeId) return;
        const found = list.find((h) => h.id === id);
        if (found) {
          const legacyType = found.webhook_type ?? "generic";
          cfg = {
            ...found,
            kind:
              found.kind === "webhook" && legacyType !== "generic"
                ? (legacyType as HookKind)
                : found.kind,
            webhook_type: legacyType,
            webhook_secret: found.webhook_secret ?? "",
            webhook_message_style: found.webhook_message_style ?? "rich",
            webhook_mention_all: found.webhook_mention_all ?? false,
          };
        }
        else {
          cfg = null;
          loadError = t("hooks.edit.notFound");
        }
      })
      .catch((e) => {
        if (id !== routeId) return; // 同上:过期请求的失败也不该污染当前页
        cfg = null;
        loadError = t("common.loadFailed", { e });
      });
  });

  async function save() {
    if (!cfg) return;
    saving = true;
    saveError = "";
    try {
      const list = await listHooks();
      const i = list.findIndex((h) => h.id === cfg!.id);
      if (i >= 0) list[i] = { ...cfg };
      else list.push({ ...cfg });
      await saveHooks(list);
      hooksStore.bump();
      if (isNew) goto(`/hooks/${cfg.id}`);
    } catch (e) {
      saveError = t("common.saveFailed", { e });
    } finally {
      saving = false;
    }
  }

  async function remove() {
    if (!cfg || isNew) return;
    const yes = await ask(t("hooks.delete.confirm", { name: cfg.name || t("hooks.unnamed") }), {
      title: t("hooks.delete.title"),
      kind: "warning",
      okLabel: t("hooks.action.delete"),
      cancelLabel: t("hooks.action.cancel"),
    });
    if (!yes) return;
    try {
      const list = (await listHooks()).filter((h) => h.id !== cfg!.id);
      await saveHooks(list);
      hooksStore.bump();
      goto("/hooks");
    } catch (e) {
      saveError = t("common.deleteFailed", { e });
    }
  }

  async function runTest() {
    if (!cfg) return;
    testing = true;
    testResult = null;
    try {
      const msg = await testHook({ ...cfg });
      testResult = { ok: true, msg };
    } catch (e) {
      testResult = { ok: false, msg: String(e) };
    } finally {
      testing = false;
    }
  }

  /** 主体未填时禁保存/测试:shell 要命令,webhook 要 URL。 */
  const bodyMissing = $derived(
    !cfg || (cfg.kind === "shell" ? !cfg.command.trim() : !cfg.url.trim()),
  );

  const webhookHelp = $derived.by(() => {
    if (!cfg) return { desc: "", hint: "", placeholder: "" };
    if (cfg.kind === "feishu") {
      return {
        desc: t("hooks.kind.feishuDesc"),
        hint: t("hooks.kind.feishuHint"),
        placeholder: "https://open.feishu.cn/open-apis/bot/v2/hook/…",
      };
    }
    if (cfg.kind === "wecom") {
      return {
        desc: t("hooks.kind.wecomDesc"),
        hint: t("hooks.kind.wecomHint"),
        placeholder: "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=…",
      };
    }
    return {
      desc: t("hooks.kind.genericDesc"),
      hint: t("hooks.kind.genericHint"),
      placeholder: "https://example.com/hook",
    };
  });
</script>

<div class="page">
  <header class="topbar"><h1>{isNew ? t("hooks.edit.newTitle") : cfg?.name || t("hooks.edit.editTitle")}</h1></header>

  {#if loadError}
    <div class="banner">{loadError}</div>
  {:else if cfg}
    {#if saveError}
      <div class="banner">{saveError}</div>
    {/if}

    <div class="rows">
      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("hooks.field.name.label")}</span>
          <span class="row-desc">{t("hooks.field.name.desc")}</span>
        </div>
        <input
          class="row-input"
          placeholder={t("hooks.field.name.placeholder")}
          bind:value={cfg.name}
          oninput={() => (testResult = null)}
        />
      </div>

      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("hooks.field.event.label")}</span>
          <span class="row-desc">{t("hooks.field.event.desc")}</span>
        </div>
        <select class="row-input" bind:value={cfg.event} onchange={() => (testResult = null)}>
          {#each HOOK_EVENTS as e (e.value)}
            <option value={e.value}>{e.label}</option>
          {/each}
        </select>
      </div>

      <div class="row">
        <div class="row-info">
          <span class="row-label">{t("hooks.field.kind.label")}</span>
          <span class="row-desc">
            {cfg.kind === "shell" ? t("hooks.kind.shellDesc") : webhookHelp.desc}
          </span>
        </div>
        <div class="segmented" role="radiogroup" aria-label={t("hooks.field.kind.label")}>
          {#each [
            ["shell", "Shell"],
            ["feishu", t("hooks.kind.feishu")],
            ["wecom", t("hooks.kind.wecom")],
            ["webhook", t("hooks.kind.generic")],
          ] as [v, label] (v)}
            <label class="seg-item">
              <input
                type="radio"
                name="kind"
                value={v}
                checked={cfg.kind === v}
                onchange={() => {
                  cfg!.kind = v as HookKind;
                  cfg!.webhook_type =
                    v === "feishu" || v === "wecom" ? v : "generic";
                  testResult = null;
                }}
              />
              {label}
            </label>
          {/each}
        </div>
      </div>

      {#if cfg.kind === "shell"}
        <div class="row column">
          <div class="row-info">
            <span class="row-label">{t("hooks.field.command.label")}</span>
            <span class="row-desc">{t("hooks.field.command.desc")}</span>
          </div>
          <textarea
            class="cmd"
            rows="3"
            placeholder={t("hooks.field.command.placeholder")}
            bind:value={cfg.command}
            oninput={() => (testResult = null)}
          ></textarea>
        </div>
      {:else}
        <div class="row">
          <div class="row-info">
            <span class="row-label">{t("hooks.field.url.label")}</span>
            <span class="row-desc">{webhookHelp.hint}</span>
          </div>
          <input
            class="row-input wide"
            placeholder={webhookHelp.placeholder}
            bind:value={cfg.url}
            oninput={() => (testResult = null)}
          />
        </div>
        {#if cfg.kind === "feishu" || cfg.kind === "wecom"}
          <div class="row">
            <div class="row-info">
              <span class="row-label">{t("hooks.field.style.label")}</span>
              <span class="row-desc">
                {cfg.kind === "feishu" ? t("hooks.style.feishuDesc") : t("hooks.style.wecomDesc")}
              </span>
            </div>
            <div class="segmented" role="radiogroup" aria-label={t("hooks.field.style.label")}>
              {#each [["rich", cfg.kind === "feishu" ? t("hooks.style.rich") : "Markdown"], ["text", t("hooks.style.plain")]] as [v, label] (v)}
                <label class="seg-item">
                  <input
                    type="radio"
                    name="message-style"
                    value={v}
                    checked={cfg.webhook_message_style === v}
                    onchange={() => {
                      cfg!.webhook_message_style = v as "text" | "rich";
                      testResult = null;
                    }}
                  />
                  {label}
                </label>
              {/each}
            </div>
          </div>

          {#if cfg.kind === "feishu"}
            <div class="row">
              <div class="row-info">
                <span class="row-label">{t("hooks.field.secret.label")}</span>
                <span class="row-desc">{t("hooks.field.secret.desc")}</span>
              </div>
              <input
                class="row-input wide"
                type="password"
                autocomplete="off"
                placeholder={t("hooks.field.secret.placeholder")}
                bind:value={cfg.webhook_secret}
                oninput={() => (testResult = null)}
              />
            </div>
          {/if}

          <label class="row">
            <div class="row-info">
              <span class="row-label">{t("hooks.field.mentionAll.label")}</span>
              <span class="row-desc">{t("hooks.field.mentionAll.desc")}</span>
            </div>
            <input type="checkbox" class="switch" bind:checked={cfg.webhook_mention_all} />
          </label>

          <div class="webhook-tip">
            <span aria-hidden="true">✓</span>
            {cfg.kind === "feishu" ? t("hooks.tip.feishu") : t("hooks.tip.wecom")}
          </div>
        {:else}
          <div class="webhook-tip">
            <span aria-hidden="true">i</span>
            {t("hooks.tip.generic")}
          </div>
        {/if}
      {/if}

      <label class="row">
        <div class="row-info">
          <span class="row-label">{t("hooks.field.includeNote.label")}</span>
          <span class="row-desc">
            {(cfg.kind === "feishu" || cfg.kind === "wecom")
              ? t("hooks.includeNote.imDesc")
              : t("hooks.includeNote.otherDesc")}
          </span>
        </div>
        <!-- 与启用开关不同:附带与否改变测试注入的内容,旧测试结果不再作数 -->
        <input
          type="checkbox"
          class="switch"
          bind:checked={cfg.include_note}
          onchange={() => (testResult = null)}
        />
      </label>

      <label class="row">
        <div class="row-info">
          <span class="row-label">{t("hooks.field.enabled.label")}</span>
          <span class="row-desc">{t("hooks.field.enabled.desc")}</span>
        </div>
        <input type="checkbox" class="switch" bind:checked={cfg.enabled} />
      </label>
    </div>

    <div class="actions">
      <button class="btn-primary" onclick={save} disabled={saving || bodyMissing}>
        {saving ? t("hooks.action.saving") : t("hooks.action.save")}
      </button>
      <button class="btn-secondary" onclick={runTest} disabled={testing || bodyMissing}>
        {testing ? t("hooks.action.testing") : t("hooks.action.test")}
      </button>
      {#if !isNew}
        <button class="btn-danger" onclick={remove}>{t("hooks.action.delete")}</button>
      {/if}
    </div>

    {#if testResult}
      <p class="test-result" class:ok={testResult.ok} class:err={!testResult.ok}>
        {testResult.ok ? t("hooks.test.ok", { msg: testResult.msg }) : t("hooks.test.err", { msg: testResult.msg })}
      </p>
    {/if}
  {/if}
</div>

<style>
  .page { padding: 0 1.5rem 2rem; }
  .topbar { position: sticky; top: 0; background: var(--canvas); padding: 1.1rem 0 0.6rem; }
  h1 { font-size: 1.15rem; font-weight: 500; margin: 0; }
  .rows {
    margin-top: 1rem;
    background: var(--surface);
    border-radius: var(--radius-lg);
    overflow: hidden;
  }
  .row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.5rem 0.9rem;
    padding: 0.55rem 1rem;
    border-bottom: 1px solid var(--hairline);
  }
  .rows .row:last-child { border-bottom: none; }
  label.row { cursor: pointer; }
  /* 多行输入行:说明在上、textarea 全宽在下 */
  .row.column { flex-direction: column; align-items: stretch; }
  .row-info { flex: 1; min-width: 11rem; display: flex; flex-direction: column; gap: 0.1rem; }
  .row-label { font-size: 0.92rem; color: var(--ink); }
  .row-desc { font-size: 0.8rem; color: var(--ink-secondary); line-height: 1.4; }
  .row-input {
    margin-left: auto;
    box-sizing: border-box;
    width: 13rem;
    padding: 0.35em 0.6em;
    border: 1px solid transparent;
    border-radius: var(--radius-md);
    background: var(--surface-press);
    color: var(--ink);
    font-size: 0.88em;
  }
  .row-input.wide { width: 20rem; max-width: 100%; }
  .row-input:focus {
    outline: none;
    background: var(--canvas);
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }
  select.row-input { cursor: pointer; }
  .cmd {
    box-sizing: border-box;
    width: 100%;
    padding: 0.5em 0.7em;
    border: 1px solid transparent;
    border-radius: var(--radius-md);
    background: var(--surface-press);
    color: var(--ink);
    font-size: 0.85em;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    line-height: 1.5;
    resize: vertical;
  }
  .cmd:focus {
    outline: none;
    background: var(--canvas);
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }
  .segmented {
    margin-left: auto;
    display: flex;
    background: var(--surface-press);
    border-radius: var(--radius-md);
    padding: 2px;
  }
  .seg-item {
    padding: 0.25em 0.7em;
    border-radius: var(--radius-md);
    font-size: 0.85em;
    color: var(--ink-secondary);
    cursor: pointer;
  }
  .seg-item:hover { color: var(--ink); }
  .seg-item:has(input:checked) {
    background: var(--canvas);
    color: var(--ink);
    box-shadow: var(--shadow-btn);
  }
  .seg-item input { position: absolute; opacity: 0; pointer-events: none; }
  .segmented { flex-wrap: wrap; justify-content: flex-end; }
  .webhook-tip {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    padding: 0.55rem 1rem;
    border-bottom: 1px solid var(--hairline);
    color: var(--ink-secondary);
    background: var(--surface-soft);
    font-size: 0.8rem;
    line-height: 1.4;
  }
  .webhook-tip span { color: var(--success, var(--ink)); font-weight: 600; }
  .actions { margin-top: 1rem; display: flex; gap: 0.6rem; align-items: center; }
  .btn-primary {
    border: none;
    border-radius: var(--radius-full);
    padding: 0.45em 1.1em;
    font-size: 0.88rem;
    font-weight: 500;
    color: var(--on-primary);
    background: var(--primary);
    box-shadow: var(--shadow-btn);
    cursor: pointer;
  }
  .btn-primary:hover { background: var(--primary-pressed); }
  .btn-primary:disabled { opacity: 0.5; cursor: default; }
  .btn-secondary {
    background: transparent;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-md);
    padding: 0.4em 0.9em;
    font-size: 0.88rem;
    color: var(--ink);
    cursor: pointer;
  }
  .btn-secondary:hover { background: var(--surface-soft); }
  .btn-secondary:disabled { opacity: 0.5; cursor: default; }
  .btn-danger {
    margin-left: auto;
    background: transparent;
    border: 1px solid var(--danger);
    border-radius: var(--radius-md);
    padding: 0.4em 0.9em;
    font-size: 0.88rem;
    color: var(--danger);
    cursor: pointer;
  }
  .btn-danger:hover { background: var(--danger-tint); }
  .test-result { font-size: 0.85rem; margin: 0.6rem 0 0; }
  .test-result.ok { color: var(--success, var(--ink-secondary)); }
  .test-result.err { color: var(--danger-ink); }
  .banner {
    background: var(--danger-tint);
    border: 1px solid var(--danger-line);
    color: var(--danger-ink);
    border-radius: var(--radius-lg);
    padding: 0.5rem 0.6rem;
    margin-top: 1rem;
    font-size: 0.85rem;
  }
</style>
