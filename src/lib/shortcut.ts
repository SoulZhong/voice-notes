// 快捷键组装/展示的纯函数;不依赖 tauri,方便单测和在设置页复用。
//
// tauri-plugin-global-shortcut 的 accelerator 语法形如 "Alt+CmdOrCtrl+R":
// 修饰键用 "+" 连接,顺序对解析不敏感,但我们固定输出顺序(Ctrl, Alt, Shift, CmdOrCtrl)
// 保证同一个按键组合每次生成的字符串完全一致,方便存储比较/去重。

// e.code → accelerator 主键名的映射表。只覆盖录入快捷键场景下常见的按键,
// 不认识的 code 一律返回 null,交给调用方提示"暂不支持这个按键"。
function mainKeyFromCode(code: string): string | null {
  // 字母键:"KeyA" → "A"
  if (code.startsWith("Key") && code.length === 4) return code.slice(3);
  // 数字键:"Digit5" → "5"
  if (code.startsWith("Digit") && code.length === 6) return code.slice(5);
  // 功能键原样透传:"F1".."F12"
  if (/^F([1-9]|1[0-2])$/.test(code)) return code;
  if (code === "Space") return "Space";
  // 其余常用可打印/编辑键
  const extra: Record<string, string> = {
    Enter: "Enter",
    Escape: "Escape",
    Tab: "Tab",
    Backspace: "Backspace",
    Delete: "Delete",
    ArrowUp: "Up",
    ArrowDown: "Down",
    ArrowLeft: "Left",
    ArrowRight: "Right",
    Minus: "-",
    Equal: "=",
    Comma: ",",
    Period: ".",
    Slash: "/",
    Semicolon: ";",
    Quote: "'",
    BracketLeft: "[",
    BracketRight: "]",
    Backquote: "`",
    Backslash: "\\",
  };
  return extra[code] ?? null;
}

/**
 * 从键盘事件组装 tauri accelerator 字符串。
 *
 * 规则:
 * - 纯修饰键(用户只按下了 Ctrl/Alt/Shift/Meta 本身,还没按主键)返回 null,
 *   等用户再按一个主键的按键事件。
 * - 没有任何修饰键的裸按键(如单按 "R")也返回 null——全局快捷键必须带至少一个
 *   修饰键,否则会和普通输入/其他软件快捷键冲突,属于误触高发区,直接在录入
 *   阶段就拦掉,不让用户录出一个裸键的全局快捷键。
 * - 不认识的主键 code 返回 null。
 */
export function acceleratorFromEvent(e: KeyboardEvent): string | null {
  // 修饰键自身触发的 keydown,此时还没有主键,不能组成合法的 accelerator。
  if (/^(Control|Alt|Shift|Meta)/.test(e.code)) return null;

  const mods: string[] = [];
  if (e.ctrlKey) mods.push("Ctrl");
  if (e.altKey) mods.push("Alt");
  if (e.shiftKey) mods.push("Shift");
  if (e.metaKey) mods.push("CmdOrCtrl");

  if (mods.length === 0) return null; // 裸键,防误触

  const mainKey = mainKeyFromCode(e.code);
  if (mainKey === null) return null;

  return [...mods, mainKey].join("+");
}

// accelerator 中的修饰键 token → mac 符号,展示时按 ⌃⌥⇧⌘ 的固定顺序重排。
// 注:DESIGN 文案规范禁止在界面文案中使用 emoji 装饰,但 ⌃⌥⇧⌘ 属于 macOS 系统
// 原生的键位符号(菜单栏、系统偏好设置里都是这么显示的),不是图标化的装饰性 emoji,
// 在"展示一个快捷键"这个语境下是用户预期的惯例写法,因此不受该规范约束。
const MODIFIER_SYMBOLS: Record<string, string> = {
  Ctrl: "⌃",
  Alt: "⌥",
  Shift: "⇧",
  CmdOrCtrl: "⌘",
};
const MODIFIER_DISPLAY_ORDER = ["Ctrl", "Alt", "Shift", "CmdOrCtrl"];

/** 把 "Alt+CmdOrCtrl+R" 这样的 accelerator 转成 "⌥⌘R" 供界面展示。 */
export function displayShortcut(acc: string): string {
  const tokens = acc.split("+");
  const modifierTokens = new Set(tokens.filter((t) => t in MODIFIER_SYMBOLS));
  const mainKey = tokens.find((t) => !(t in MODIFIER_SYMBOLS)) ?? "";

  const symbols = MODIFIER_DISPLAY_ORDER.filter((m) => modifierTokens.has(m)).map(
    (m) => MODIFIER_SYMBOLS[m],
  );

  return [...symbols, mainKey].join("");
}
