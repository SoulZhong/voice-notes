// 笔记页书签 widget 的 DOM(会议备注)。由 memoDecorations 的装饰调用,Svelte 之外的
// 纯 DOM 构建(NodeView 同法);文案一律 tStatic——widget 在创建它的 effect 销毁后仍会被
// PM 调用,普通 t() 会触发 derived_inert 并中断渲染(见 i18n 的 tStatic 注释)。
//
// 状态纪律:书签只渲染后端给的备注;改/删/调时间都交给页面(onAction),成功后由
// memos_changed 整表刷新、widget 因 key(含 updated_at)变化整只重建。本地只持有
// 「正在编辑/调时间」这类纯界面态,随 DOM 重建自然丢弃。
import { tStatic } from "$lib/i18n/index.svelte";
import { fmtClock, parseClock, MEMO_MAX_CHARS, type Memo } from "../memos";
import type { MemoAction, MemoDraft, MemoPlacementItem } from "./memoDecorations";

export type MemoWidgetCallbacks = {
  onAction: (a: MemoAction) => Promise<unknown> | void;
};

const FLAG_SVG =
  '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3.5 14.5V2"/><path d="M3.5 2.5h8.2l-1.9 2.9 1.9 2.9H3.5"/></svg>';

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls: string, text?: string): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function btn(cls: string, text: string, onClick: (e: MouseEvent) => void, title?: string): HTMLButtonElement {
  const b = el("button", cls, text);
  b.type = "button";
  if (title) b.title = title;
  b.onclick = (e) => {
    e.preventDefault();
    onClick(e);
  };
  return b;
}

function flag(): HTMLSpanElement {
  const f = el("span", "memo-bm-flag");
  f.innerHTML = FLAG_SVG;
  return f;
}

/** 单行起步、随内容长高的输入框(备注可多行:Shift+回车换行)。 */
function autoTextarea(value: string, placeholder: string): HTMLTextAreaElement {
  const ta = el("textarea", "memo-bm-input");
  ta.rows = 1;
  ta.maxLength = MEMO_MAX_CHARS;
  ta.value = value;
  ta.placeholder = placeholder;
  const fit = () => {
    ta.style.height = "auto";
    ta.style.height = `${ta.scrollHeight}px`;
  };
  ta.addEventListener("input", fit);
  setTimeout(fit, 0);
  return ta;
}

/** 回调结果统一成 Promise:void 视为成功。 */
function run(cb: MemoWidgetCallbacks, a: MemoAction): Promise<unknown> {
  try {
    return Promise.resolve(cb.onAction(a));
  } catch (e) {
    return Promise.reject(e);
  }
}

function focusSoon(ta: HTMLTextAreaElement) {
  // PM 在 toDOM 返回后才把 DOM 挂进文档;挂上之后再聚焦(并让浏览器把它滚进视野)。
  setTimeout(() => {
    if (!ta.isConnected) return;
    ta.focus();
    ta.setSelectionRange(ta.value.length, ta.value.length);
  }, 0);
}

export function renderMemoWidget(item: MemoPlacementItem, cb: MemoWidgetCallbacks): HTMLElement {
  return item.kind === "memo" ? renderMemo(item.memo, item.cut, cb) : renderDraft(item.draft, cb);
}

function renderMemo(memo: Memo, cut: boolean, cb: MemoWidgetCallbacks): HTMLElement {
  const root = el("div", "memo-bm");
  root.contentEditable = "false";
  root.dataset.memoId = memo.id;

  const time = btn("memo-bm-time", fmtClock(memo.at_ms), () => toggleAdjust(), tStatic("memo.action.adjust"));
  const body = el("span", memo.text ? "memo-bm-text" : "memo-bm-text marker", memo.text || tStatic("memo.marker"));
  const main = el("span", "memo-bm-main");
  main.append(body);
  if (cut) main.append(el("span", "memo-bm-cut", tStatic("memo.cut")));

  const actions = el("span", "memo-bm-actions");
  actions.append(
    btn("memo-bm-act", tStatic("memo.action.play"), () => void run(cb, { kind: "play", memo }).catch(() => {})),
    btn("memo-bm-act", tStatic("memo.action.edit"), () => startEdit()),
    btn("memo-bm-act", tStatic("memo.action.delete"), () => void run(cb, { kind: "delete", memo }).catch(() => {})),
  );
  root.append(flag(), time, main, actions);

  // ── 改文字:正文原位换成输入框;回车保存、Shift+回车换行、Esc 放弃 ──
  let editor: HTMLElement | null = null;
  function startEdit() {
    if (editor) return;
    closeAdjust();
    const ta = autoTextarea(memo.text, tStatic("memo.note.draftPlaceholder"));
    const row = el("span", "memo-bm-editrow");
    const save = btn("memo-bm-act strong", tStatic("memo.save"), () => commit());
    const cancel = btn("memo-bm-act", tStatic("memo.cancel"), () => stopEdit());
    row.append(save, cancel);
    editor = el("span", "memo-bm-edit");
    editor.append(ta, row);
    main.replaceWith(editor);
    actions.hidden = true;
    root.classList.add("editing");
    let busy = false;
    function commit() {
      if (busy) return;
      const text = ta.value.replace(/\s+$/, "");
      if (text === memo.text) {
        stopEdit();
        return;
      }
      busy = true;
      ta.disabled = save.disabled = true;
      run(cb, { kind: "edit", memo, text }).then(
        () => {
          // 文字没变时后端可能不改 updated_at,widget 不重建——本地先收起并显示新值。
          body.textContent = text || tStatic("memo.marker");
          body.classList.toggle("marker", !text);
          stopEdit();
        },
        () => {
          busy = false;
          ta.disabled = save.disabled = false;
          ta.focus();
        },
      );
    }
    ta.addEventListener("keydown", (e) => {
      if (e.isComposing) return;
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        commit();
      } else if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        stopEdit();
      }
    });
    focusSoon(ta);
  }
  function stopEdit() {
    if (!editor) return;
    editor.replaceWith(main);
    editor = null;
    actions.hidden = false;
    root.classList.remove("editing");
  }

  // ── 调时间:点时间开小浮层,−5 秒 / +5 秒 微调输入框里的值,确定才写 ──
  let pop: HTMLElement | null = null;
  function toggleAdjust() {
    if (pop) closeAdjust();
    else openAdjust();
  }
  function closeAdjust() {
    pop?.remove();
    pop = null;
  }
  function openAdjust() {
    stopEdit();
    pop = el("div", "memo-bm-adjust");
    pop.setAttribute("role", "dialog");
    pop.setAttribute("aria-label", tStatic("memo.action.adjust"));
    const input = el("input", "memo-bm-clock");
    input.type = "text";
    input.value = fmtClock(memo.at_ms);
    input.inputMode = "numeric";
    const err = el("div", "memo-bm-err");
    err.hidden = true;
    const nudge = (d: number) => {
      const cur = parseClock(input.value) ?? memo.at_ms;
      input.value = fmtClock(Math.max(0, cur + d));
      err.hidden = true;
    };
    let busy = false;
    const apply = () => {
      if (busy) return;
      const ms = parseClock(input.value);
      if (ms === null) {
        err.textContent = tStatic("memo.adjust.invalid");
        err.hidden = false;
        return;
      }
      // fmtClock 只到秒:没动过输入框(秒级相同)就不写,免得把毫秒抹掉。
      if (Math.floor(ms / 1000) === Math.floor(memo.at_ms / 1000)) {
        closeAdjust();
        return;
      }
      busy = true;
      run(cb, { kind: "adjust", memo, at_ms: ms }).then(
        () => closeAdjust(),
        () => (busy = false),
      );
    };
    input.addEventListener("keydown", (e) => {
      if (e.isComposing) return;
      if (e.key === "Enter") {
        e.preventDefault();
        apply();
      } else if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        closeAdjust();
      }
    });
    const row = el("div", "memo-bm-adjrow");
    row.append(
      btn("memo-bm-act", tStatic("memo.adjust.minus5"), () => nudge(-5000)),
      input,
      btn("memo-bm-act", tStatic("memo.adjust.plus5"), () => nudge(5000)),
    );
    const foot = el("div", "memo-bm-adjrow");
    foot.append(
      btn("memo-bm-act strong", tStatic("memo.adjust.apply"), apply),
      btn("memo-bm-act", tStatic("memo.adjust.cancel"), closeAdjust),
    );
    pop.append(row, err, foot);
    root.append(pop);
    setTimeout(() => {
      if (input.isConnected) input.select();
    }, 0);
  }

  return root;
}

function renderDraft(draft: MemoDraft, cb: MemoWidgetCallbacks): HTMLElement {
  const root = el("div", "memo-bm memo-bm-draft");
  root.contentEditable = "false";
  root.dataset.memoDraft = String(draft.seq);
  const time = el("span", "memo-bm-time static", fmtClock(draft.at_ms));
  const ta = autoTextarea("", tStatic("memo.note.draftPlaceholder"));
  const row = el("span", "memo-bm-editrow");
  let busy = false;
  const save = () => {
    if (busy) return;
    busy = true;
    ta.disabled = true;
    run(cb, { kind: "draftSave", at_ms: draft.at_ms, text: ta.value.replace(/\s+$/, "") }).catch(() => {
      busy = false;
      ta.disabled = false;
      ta.focus();
    });
  };
  const cancel = () => void run(cb, { kind: "draftCancel" }).catch(() => {});
  row.append(btn("memo-bm-act strong", tStatic("memo.save"), save), btn("memo-bm-act", tStatic("memo.cancel"), cancel));
  const edit = el("span", "memo-bm-edit");
  edit.append(ta, row);
  ta.addEventListener("keydown", (e) => {
    if (e.isComposing) return;
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      save();
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      cancel();
    }
  });
  root.append(flag(), time, edit);
  focusSoon(ta);
  return root;
}
