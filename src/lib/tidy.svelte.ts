// 整理收件箱的共享会话态:侧栏徽标、概览摘要卡、审阅流、详情页 ctx 提示四处同源。
// refresh 即"自动归并 + 拉剩余":高置信建议在后端落日志合并(录制中后端只读),
// 返回的 remaining 才是要人工拍板的。人工处置(忽略建议对/保留无样本/忽略同名组)
// 落盘(merge_journal/dismissed.json,上限 500):重启后不再重现——用户处理过的
// 条目反复出现是可用性问题,原"忽略不落盘防藏真重复"的权衡被真实使用推翻;详情页
// 手动合并入口仍在,藏不住真重复。
import {
  applyConfidentMerges,
  dismissTidyItem,
  listDismissedTidyItems,
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
  /** 手动合并后的撤销条(最近一次,会话级全局)。手动合并不进回执收件箱,这里是
      UI 上唯一的撤销入口——挂在页面局部状态时一次导航就永久丢失(后端日志明明
      还在),故提到 store:概览页与人物详情页同读同写,谁合并的都能在两处撤。 */
  lastManual = $state<{ journalId: string; label: string } | null>(null);
  /** 人工处置集(忽略的建议对/保留的无样本条目/忽略的同名组),键=tidyItemKey。
      落盘为真值源,本地 Set 是即时展示的乐观镜像。 */
  dismissed = $state<Set<string>>(new Set());
  loading = $state(false);
  private inflight: Promise<void> | null = null;

  /** 未被处置的建议(展示/计数用)。 */
  get visible(): PersonMergeSuggestion[] {
    return this.suggestions.filter((s) => !this.dismissed.has(`s:${sugKey(s)}`));
  }

  /** 与某人相关的建议(详情页上下文提示用)。 */
  involving(personId: string): PersonMergeSuggestion[] {
    return this.visible.filter((s) => s.loser === personId || s.winner === personId);
  }

  ignore(s: PersonMergeSuggestion) {
    this.dismiss(`s:${sugKey(s)}`);
  }

  /** 本地即时生效 + 落盘 best-effort(失败顶多重启后再见一次,不影响本次会话)。 */
  dismiss(key: string) {
    this.dismissed = new Set([...this.dismissed, key]);
    void dismissTidyItem(key);
  }

  /** 乐观移除一条回执(「好」/撤销点击后立即收起卡片,不等后台整轮重算对账)。 */
  removeReceipt(journalId: string) {
    this.receipts = this.receipts.filter((r) => r.journal_id !== journalId);
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
      // 与主流程并行拉服务端处置名单(重启后的持久化真值);失败不影响本轮
      // 整理——处置名单是增值层,拿不到顶多本地已知的这些还在生效。
      const [outcome, dismissedFromServer] = await Promise.all([
        applyConfidentMerges(),
        listDismissedTidyItems().catch(() => [] as string[]),
      ]);
      this.suggestions = outcome.remaining;
      this.receipts = await listMergeReceipts();
      if (dismissedFromServer.length > 0) {
        this.dismissed = new Set([...this.dismissed, ...dismissedFromServer]);
      }
      if (outcome.applied.length > 0) recording.bumpPeople();
    } catch {
      this.suggestions = [];
      this.receipts = [];
    }
    this.loading = false;
  }
}

export const tidy = new TidyState();
