import { invoke } from "@tauri-apps/api/core";

/** 一条开录前风险。`kind` 是稳定机读标识(后端 precheck.rs 的常量),按它选文案。 */
export type RecordRisk = { kind: string; detail: string };

/**
 * 滤掉本次应用会话内已被「仍然录制」放行的 kind。
 *
 * 为什么要有这一层:固定用蓝牙麦克风的人每次开录都被拦,会训练出闭眼点击——
 * 那就退化成了 2026-08-17 那次失效的横幅。放行只在内存里记,不落盘,重启后重新
 * 提醒,不给永久静音的出口。
 */
export function unresolvedRisks(
  risks: RecordRisk[],
  dismissed: ReadonlySet<string>,
): RecordRisk[] {
  return risks.filter((r) => !dismissed.has(r.kind));
}

/**
 * 开录守卫。`guard()` 返回 true 表示可以开录。
 *
 * 探测失败一律放行:查不到系统状态是我们的问题,不能因此让人录不了会——这与后端
 * `mic_mode` 读不到时回落 Unknown 不提示是同一条原则。
 */
class RecordRiskGate {
  /** 待用户决定的风险;非空即对话框应显示。 */
  risks = $state<RecordRisk[]>([]);
  /** 本次应用会话内已放行的 kind。刻意只在内存里,不落盘。 */
  #dismissed = new Set<string>();
  #resolve: ((go: boolean) => void) | null = null;
  /** 进行中的那次确认。并发调用共用它,不各起一次。 */
  #pending: Promise<boolean> | null = null;
  #probe: () => Promise<RecordRisk[]>;

  constructor(probe: () => Promise<RecordRisk[]>) {
    this.#probe = probe;
  }

  /**
   * 并发调用返回**同一个** Promise(Codex review P1):原先每次都覆盖 `#resolve`,
   * 双击按钮、或侧栏与录制页先后触发时,前一个 Promise 永远不会 settle,那条开录
   * 流程就永久挂起了——而按钮在 guard() 期间并没有 pending 态,这条竞态实际可达。
   *
   * 注意本方法不是 async:`#pending` 必须在**同步**阶段就位,否则两次快速调用会
   * 双双越过判空、各起一次探测。
   */
  guard(): Promise<boolean> {
    if (!this.#pending) {
      this.#pending = this.#run().finally(() => {
        this.#pending = null;
      });
    }
    return this.#pending;
  }

  async #run(): Promise<boolean> {
    let found: RecordRisk[];
    try {
      found = unresolvedRisks(await this.#probe(), this.#dismissed);
    } catch {
      return true;
    }
    if (found.length === 0) return true;
    this.risks = found;
    return new Promise<boolean>((r) => {
      this.#resolve = r;
    });
  }

  /** 用户选「仍然录制」:放行并把这几条记进本次会话的免打扰名单。 */
  proceed() {
    for (const r of this.risks) this.#dismissed.add(r.kind);
    this.risks = [];
    this.#resolve?.(true);
    this.#resolve = null;
  }

  /** 用户选「去改设置」:不开录。名单不动——他改完设置回来,风险自然消失。 */
  cancel() {
    this.risks = [];
    this.#resolve?.(false);
    this.#resolve = null;
  }
}

export function createRecordRiskGate(probe: () => Promise<RecordRisk[]>) {
  return new RecordRiskGate(probe);
}

/** 应用级单例:两个开录入口共用同一个门与同一份免打扰名单。 */
export const recordRiskGate = createRecordRiskGate(() =>
  invoke<RecordRisk[]>("precheck_recording"),
);
