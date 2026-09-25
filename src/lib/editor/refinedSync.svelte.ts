// 修订稿同步:笔记页里「修订稿从哪来、编辑器什么时候跟上、保存怎么排队、冲突怎么收场」
// 的全部规则。此前这些散在 +page.svelte 与 MarkdownEditor 之间约 200 行,每条规则都是
// 一次真机事故或一轮 Codex 审查换来的,但只能挂上整页才测得到(唯一的测试靠 grep 页面
// 源码)。收进这里之后,页面只负责把 $effect 接到下面几个方法上;规则经 ports 注入的
// 假后端即可逐条测试(refinedSync.test.ts)。
//
// 四类写者都要改 doc:页面 refresh、Aing 终态/在跑复核、编辑器保存后的回读、实体编辑后
// 的重取。它们各自 await,谁先发起不代表谁先落地——所以取稿一律走 beginLoad/commit/
// endLoad 三件套(末次请求赢)。

import { untrack } from "svelte";
import type { ParagraphPayload, RefinedDoc } from "$lib/notes";
import { rebaseQueuedRefinedSave } from "./editorDoc";

export type RefinedSaveSnapshot = { revision: number; paragraphs: ParagraphPayload[] };

/** 后端两个命令。测试注入假实现。 */
export interface RefinedSyncPorts {
  getRefined(id: string): Promise<RefinedDoc | null>;
  saveRefined(id: string, revision: number, paragraphs: ParagraphPayload[]): Promise<number>;
}

/** 编辑器里本模块要用到的那几个方法(MarkdownEditor 实例满足它)。 */
export interface RefinedEditorHandle {
  hasFocus(): boolean;
  setRefined(doc: RefinedDoc): void;
  markSaved(revision: number): void;
  markSaveFailed(): void;
}

/** 页面提供的现场:当前路由的笔记、当前编辑器实例、报错文案。 */
export interface RefinedSyncHost {
  currentId(): string;
  editor(): RefinedEditorHandle | null;
  saveFailedText(err: unknown): string;
  drainFailedText(err: unknown): string;
}

type ActiveSave = { payload: RefinedSaveSnapshot; done: Promise<number> };

/** 乐观并发冲突:后端错误原文判等。 */
const CONFLICT_MARK = "已在别处更新"; // i18n-exempt: 与后端错误原文判等

export class RefinedSync {
  /** 页面展示用的修订稿。 */
  doc = $state<RefinedDoc | null>(null);
  /** 取稿在途数。在途期间不出「这场没做 AI 整理」——眼前这份随时可能被换掉。 */
  loading = $state(0);
  /** 最新一次取稿失败,手里这份可能过期:提示一律不出,直到某次重取成功。 */
  stale = $state(false);
  /** 保存错误(粘性去重:按 2s 重试的持续性拒绝不该每次刷新一条)。成功保存后清空。 */
  saveErr = $state("");
  /**
   * 编辑器已同步到的那份稿(非响应式,刻意:读它的 effect 不该因它重跑)。
   * 与下面的 syncedEditor 配对构成身份闸门:保存成功后 doc 换新对象身份必然触发
   * 同步 effect,若不闸,失焦保存场景会用旧段落快照重建编辑器,吹掉用户紧接着的
   * 输入。两者必须配对判定(Fix Round 2):只闸 doc 的话,视图来回切换时编辑器被
   * 销毁重挂出新实例、doc 却没变,新实例永远收不到 setRefined,渲染空白且其中打字
   * 静默不落盘。
   */
  synced: RefinedDoc | null = null;
  private syncedEditor: unknown = null;
  /** 编辑器里加载的是哪篇的稿(不是路由 id):切笔记时 flush 必须落到旧笔记。 */
  private loadedId: string | null = null;
  private seq = 0;
  /** 保存排队活在这里而不在编辑器里,不依赖即将销毁的编辑器实例。每篇至多一份最新快照。 */
  private active = new Map<string, ActiveSave>();
  private pending = new Map<string, RefinedSaveSnapshot>();
  private running = new Set<string>();

  constructor(
    private readonly ports: RefinedSyncPorts,
    private readonly host: RefinedSyncHost,
  ) {}

  /** 切笔记:复位展示态。loadedId 刻意不清——复位前的 flush 还要靠它找到旧笔记。 */
  reset() {
    this.doc = null;
    this.stale = false;
    this.saveErr = "";
    this.synced = null;
    this.syncedEditor = null;
  }

  /**
   * 占号 + 计入在途,必须在 await **之前**:否则一份早发出、慢回来的旧回读会拿到更大的
   * 号把新稿盖掉。每个 beginLoad 配一个 endLoad(放 finally)。
   *
   * untrack 不可省:`loading += 1` 是"读 + 写",而页面 refresh() 是在 $effect 里**同步**
   * 调到这里的——不 untrack 那个 effect 会把 loading 记成依赖又亲手改它,自我失效成死
   * 循环,整页停在"加载中"(2026-08-16 真机撞到)。
   */
  private beginLoad(): number {
    untrack(() => {
      this.loading += 1;
    });
    this.seq += 1;
    return this.seq;
  }

  /** committed=false 且自己仍是最新请求 → 手里这份可能过期(被更新请求顶掉的不算)。 */
  private endLoad(seq: number, committed: boolean) {
    untrack(() => {
      this.loading -= 1;
    });
    if (!committed && seq === this.seq) this.stale = true;
  }

  /** 仍是最新请求且没切走笔记才落地。返回 false 的那份稿绝不能推给编辑器。 */
  private commit(seq: number, doc: RefinedDoc | null, forId: string): boolean {
    if (seq !== this.seq || forId !== this.host.currentId()) return false;
    this.doc = doc;
    this.stale = false;
    return true;
  }

  private markSynced(doc: RefinedDoc, editor: unknown) {
    this.synced = doc;
    this.syncedEditor = editor;
  }

  /** 取稿(页面 refresh、Aing 终态/复核)。不碰编辑器:由同步 effect 跟上。 */
  async load(forId: string): Promise<void> {
    const seq = this.beginLoad();
    let ok = false;
    try {
      ok = this.commit(seq, await this.ports.getRefined(forId), forId);
    } catch {
      /* 增值层:取不到就维持现状,过期与否交给 endLoad 判 */
    } finally {
      this.endLoad(seq, ok);
    }
  }

  /** 重取并直接推给编辑器(实体编辑后,正文高亮要立刻跟上)。编辑器有焦点时不推。 */
  async reloadIntoEditor(forId: string): Promise<void> {
    const seq = this.beginLoad();
    let ok = false;
    try {
      const latest = await this.ports.getRefined(forId);
      if (latest && forId === this.host.currentId()) {
        ok = this.commit(seq, latest, forId);
        const ed = this.host.editor();
        if (ok && !ed?.hasFocus()) {
          this.markSynced(latest, ed);
          ed?.setRefined(latest);
        }
      }
    } finally {
      this.endLoad(seq, ok);
    }
  }

  /**
   * 同步 effect 的本体:doc 或编辑器实例变了 → 重建编辑器文档。已同步过的(同一份 doc
   * + 同一个实例)跳过;正在打字不打断。页面负责只在修订稿视图下调用。
   */
  syncEditor(editor: RefinedEditorHandle | null, doc: RefinedDoc | null) {
    if (!editor || !doc) return;
    if (doc === this.synced && editor === this.syncedEditor) return;
    if (editor.hasFocus()) return;
    editor.setRefined(doc);
    this.markSynced(doc, editor);
    this.loadedId = this.host.currentId();
  }

  /** 编辑器的自动保存回调。 */
  async save(payload: RefinedSaveSnapshot): Promise<void> {
    const targetId = this.loadedId ?? this.host.currentId();
    const done = this.ports.saveRefined(targetId, payload.revision, payload.paragraphs);
    const active: ActiveSave = { payload, done };
    this.active.set(targetId, active);
    try {
      const newRev = await done;
      // await 期间编辑器已切到别的笔记:回执打在旧笔记上,编辑器切笔记时的整份载入
      // 早已复位过它的保存状态,直接丢弃。
      if (this.loadedId !== targetId) return;
      // 先 markSaved 再回读:避免编辑器 revision 出现空窗(空窗里的下一次自动保存会
      // 拿旧 revision 撞乐观并发冲突)。
      this.host.editor()?.markSaved(newRev);
      // 回读盘上最新稿让 doc 说真话(不本地拼 {...doc, revision}:那样段落仍是保存前
      // 的旧内容,与编辑器对不上——Fix Round 2 之前的做法)。
      const seq = this.beginLoad();
      let ok = false;
      try {
        const latest = await this.ports.getRefined(targetId);
        if (targetId === this.host.currentId() && this.loadedId === targetId && latest) {
          ok = this.commit(seq, latest, targetId);
          if (ok) this.markSynced(latest, this.host.editor());
        }
        // latest 为 null(如笔记目录被清):保持原样,不因一次回读失败清空整篇。
      } catch {
        /* 回读失败:doc 保持原状;保存本身已成功 */
      } finally {
        this.endLoad(seq, ok);
      }
      if (this.saveErr) this.saveErr = "";
    } catch (err) {
      if (this.loadedId !== targetId) return;
      // 必须无条件调用(即使随后走冲突重载):否则一次拒绝就让编辑器的保存状态卡死,
      // 自动保存永久停摆。
      this.host.editor()?.markSaveFailed();
      const msg = this.host.saveFailedText(err);
      if (msg !== this.saveErr) this.saveErr = msg;
      // 乐观并发冲突:当前编辑已落空,重载盘上最新内容重建文档。其他失败(Aing 中/
      // 录制中被拒)只留提示,由编辑器按空闲定时器重试。
      if (String(err).includes(CONFLICT_MARK)) {
        const seq = this.beginLoad();
        let ok = false;
        try {
          const latest = await this.ports.getRefined(targetId);
          if (targetId === this.host.currentId() && this.loadedId === targetId) {
            ok = this.commit(seq, latest, targetId);
          }
          if (ok && latest) {
            const ed = this.host.editor();
            ed?.setRefined(latest);
            this.markSynced(latest, ed);
          }
        } catch {
          /* 重载失败:保持错误横幅 */
        } finally {
          this.endLoad(seq, ok);
        }
      }
    } finally {
      if (this.active.get(targetId) === active) this.active.delete(targetId);
    }
  }

  /**
   * 编辑器卸载/切笔记前的收尾保存(detached)。与在途保存串行:在途那份落定后按其新
   * revision 重基再发,排空后若仍停在本篇且没在打字,把盘上最终状态同步回来。
   */
  queueDrain(payload: RefinedSaveSnapshot) {
    const targetId = this.loadedId ?? this.host.currentId();
    this.pending.set(targetId, payload);
    if (this.running.has(targetId)) return;
    void this.drainAfterActive(targetId, this.active.get(targetId) ?? null);
  }

  private async drainAfterActive(targetId: string, initial: ActiveSave | null) {
    this.running.add(targetId);
    let revision: number | null = null;
    let previous: ParagraphPayload[] | null = null;
    try {
      if (initial) {
        revision = await initial.done;
        previous = initial.payload.paragraphs;
      }
      while (this.pending.has(targetId)) {
        const queued = this.pending.get(targetId)!;
        this.pending.delete(targetId);
        const next =
          revision !== null && previous
            ? rebaseQueuedRefinedSave(revision, previous, queued.paragraphs)
            : queued;
        revision = await this.ports.saveRefined(targetId, next.revision, next.paragraphs);
        previous = next.paragraphs;
      }
      if (targetId === this.host.currentId() && !this.host.editor()?.hasFocus()) {
        const seq = this.beginLoad();
        let ok = false;
        try {
          const latest = await this.ports.getRefined(targetId);
          const ed = this.host.editor();
          if (latest && targetId === this.host.currentId() && !ed?.hasFocus()) {
            ok = this.commit(seq, latest, targetId);
            if (ok) {
              this.markSynced(latest, ed);
              ed?.setRefined(latest);
            }
          }
        } finally {
          this.endLoad(seq, ok);
        }
      }
    } catch (err) {
      const msg = this.host.drainFailedText(err);
      if (targetId === this.host.currentId() && msg !== this.saveErr) this.saveErr = msg;
    } finally {
      this.running.delete(targetId);
      // 排空结束的极窄窗口里可能又收到一份更新快照,继续下一轮而不丢它。
      if (this.pending.has(targetId)) {
        void this.drainAfterActive(targetId, this.active.get(targetId) ?? null);
      }
    }
  }
}
