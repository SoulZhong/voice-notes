// 钩子配置的共享数据层:类型 + IPC 封装 + 版本信号。版本信号与 recording 的
// notesVersion 同套路——编辑页保存后 bump,侧栏列表 $effect 依赖重拉,不搞事件总线。
import { invoke } from "@tauri-apps/api/core";
import { t } from "$lib/i18n/index.svelte";

export type HookKind = "shell" | "feishu" | "wecom" | "webhook";
export type WebhookType = "generic" | "feishu" | "wecom";
export type WebhookMessageStyle = "text" | "rich";

export type HookCfg = {
  id: string;
  name: string;
  event: string;
  kind: HookKind;
  command: string;
  url: string;
  webhook_type: WebhookType;
  webhook_secret: string;
  webhook_message_style: WebhookMessageStyle;
  webhook_mention_all: boolean;
  enabled: boolean;
  include_note: boolean;
};

/** 事件白名单(与后端 HookEvent::as_str 逐字对齐,顺序即下拉顺序)。
 * label 用 getter 走 i18n 字典:读取时才求值,模板/$derived 里访问会追踪 locale,切语言即更新。 */
export const HOOK_EVENTS: { value: string; label: string }[] = [
  { value: "recording_started", get label() { return t("hooks.event.recordingStarted"); } },
  { value: "recording_stopped", get label() { return t("hooks.event.recordingStopped"); } },
  { value: "recording_paused", get label() { return t("hooks.event.recordingPaused"); } },
  { value: "recording_resumed", get label() { return t("hooks.event.recordingResumed"); } },
  { value: "refine_started", get label() { return t("hooks.event.refineStarted"); } },
  { value: "refine_finished", get label() { return t("hooks.event.refineFinished"); } },
];

export const eventLabel = (v: string) => HOOK_EVENTS.find((e) => e.value === v)?.label ?? v;

class HooksState {
  version = $state(0);
  bump() {
    this.version++;
  }
}

export const hooks = new HooksState();

export async function listHooks(): Promise<HookCfg[]> {
  return await invoke("list_hooks");
}

export async function saveHooks(list: HookCfg[]): Promise<void> {
  await invoke("save_hooks", { hooks: list });
}

export async function testHook(cfg: HookCfg): Promise<string> {
  return await invoke("test_hook", { cfg });
}

/** 新建空白配置:停录是最常用触发点,作默认。 */
export function newHook(): HookCfg {
  return {
    id: "h_" + crypto.randomUUID().slice(0, 8),
    name: "",
    event: "recording_stopped",
    kind: "shell",
    command: "",
    url: "",
    webhook_type: "generic",
    webhook_secret: "",
    webhook_message_style: "rich",
    webhook_mention_all: false,
    enabled: true,
    include_note: false,
  };
}
