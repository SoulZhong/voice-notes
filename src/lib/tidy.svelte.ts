// 整理建议的共享会话态:侧栏「概览与整理」徽标、概览页整理卡、人物详情页上下文
// 提示三处同源——同一份建议列表、同一个「忽略」集合,任何一处忽略/合并,三处同步。
// 忽略只在本次运行内生效(不落盘):建议本就随库变化重算,持久忽略反而会藏住真重复。
import { suggestPersonMerges, type PersonMergeSuggestion } from "$lib/people";

export const sugKey = (s: PersonMergeSuggestion) => `${s.loser}>${s.winner}`;

class TidyState {
  suggestions = $state<PersonMergeSuggestion[]>([]);
  ignored = $state<Set<string>>(new Set());
  loading = $state(false);

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

  /** 重算建议(库变化后调用:进人物页签、合并/删除/改名后)。失败静默清空——
      建议是增值层,比对失败不该打扰主流程。 */
  async refresh() {
    this.loading = true;
    try {
      this.suggestions = await suggestPersonMerges();
    } catch {
      this.suggestions = [];
    }
    this.loading = false;
  }
}

export const tidy = new TidyState();
