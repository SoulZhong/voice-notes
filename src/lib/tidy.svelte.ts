// 整理收件箱的共享会话态:侧栏徽标、概览摘要卡、审阅流、详情页 ctx 提示四处同源。
// refresh 即"自动归并 + 拉剩余":高置信建议在后端落日志合并(录制中后端只读),
// 返回的 remaining 才是要人工拍板的。忽略(建议)与保留/忽略(无样本/同名组)只在
// 本次运行内生效——建议随库重算,持久忽略会藏住真重复;真正要"永不再犯"的是
// 撤销,它在后端落盘拒绝名单。
import {
  applyConfidentMerges,
  listMergeReceipts,
  type MergeReceipt,
  type PersonMergeSuggestion,
} from "$lib/people";
import { recording } from "$lib/recording.svelte";

export const sugKey = (s: PersonMergeSuggestion) => `${s.loser}>${s.winner}`;

/** "很可能"判定:裸余弦够高(绝对档 0.74),或 S-Norm 显著性够强(z≥3)。
    与后端 SUGGEST_STRONG_RAW / SUGGEST_STRONG_Z 同值(自动归并同一准入)。 */
export const isStrong = (s: PersonMergeSuggestion) =>
  s.similarity >= 0.74 || (s.salience ?? 0) >= 3.0;

class TidyState {
  suggestions = $state<PersonMergeSuggestion[]>([]);
  receipts = $state<MergeReceipt[]>([]);
  ignored = $state<Set<string>>(new Set());
  /** 会话级忽略/保留集(同名组「忽略」、无样本「保留」),键=tidyItemKey。 */
  dismissed = $state<Set<string>>(new Set());
  loading = $state(false);
  private inflight: Promise<void> | null = null;

  /** 未被忽略的建议(展示/计数用)。 */
  get visible(): PersonMergeSuggestion[] {
    return this.suggestions.filter((s) => !this.ignored.has(sugKey(s)));
  }

  /** 与某人相关的建议(详情页上下文提示用)。 */
  involving(personId: string): PersonMergeSuggestion[] {
    return this.visible.filter((s) => s.loser === personId || s.winner === personId);
  }

  ignore(s: PersonMergeSuggestion) {
    this.ignored = new Set([...this.ignored, sugKey(s)]);
  }

  dismiss(key: string) {
    this.dismissed = new Set([...this.dismissed, key]);
  }

  /** 自动归并 + 重拉(启动、录制停止、库变化后调用)。失败静默清空——整理是
      增值层,比对失败不该打扰主流程。有自动合并发生时 bumpPeople 驱动全局刷新
      (再触发的下一轮 refresh 不会再有 applied,自然收敛)。 */
  async refresh() {
    if (this.inflight) return this.inflight;
    this.inflight = this.doRefresh().finally(() => (this.inflight = null));
    return this.inflight;
  }

  /** 并发调用合并到同一趟(动作后 bumpPeople 与 layout effect 会双触发,
      双发第二趟拿旧库快照算 remaining 会短暂显示已消失的人)。 */
  private async doRefresh() {
    this.loading = true;
    try {
      const outcome = await applyConfidentMerges();
      this.suggestions = outcome.remaining;
      this.receipts = await listMergeReceipts();
      if (outcome.applied.length > 0) recording.bumpPeople();
    } catch {
      this.suggestions = [];
      this.receipts = [];
    }
    this.loading = false;
  }
}

export const tidy = new TidyState();
