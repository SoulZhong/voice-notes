// 钩子配置的共享数据层:类型 + IPC 封装 + 版本信号。版本信号与 recording 的
// notesVersion 同套路——编辑页保存后 bump,侧栏列表 $effect 依赖重拉,不搞事件总线。
import { invoke } from "@tauri-apps/api/core";

export type HookKind = "shell" | "webhook";

export type HookCfg = {
  id: string;
  name: string;
  event: string;
  kind: HookKind;
  command: string;
  url: string;
  enabled: boolean;
};

/** 事件白名单(与后端 HookEvent::as_str 逐字对齐,顺序即下拉顺序)。 */
export const HOOK_EVENTS: { value: string; label: string }[] = [
  { value: "recording_started", label: "录制开始" },
  { value: "recording_stopped", label: "录制停止" },
  { value: "recording_paused", label: "录制暂停" },
  { value: "recording_resumed", label: "录制恢复" },
  { value: "refine_started", label: "精修开始" },
  { value: "refine_finished", label: "精修完成" },
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
    enabled: true,
  };
}
