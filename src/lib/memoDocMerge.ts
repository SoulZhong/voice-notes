// 备注文档的三方合并(按行):编辑器有没存的改动时,别处(快捷键/浮窗/另一个窗口/MCP)
// 又改了盘上的 memo.md。base = 编辑器上次与盘对齐时的内容,local = 编辑器现在的内容,
// remote = 盘上现在的内容。三者都应是同一序列化器的输出(行格式一致),否则会多出假差异。
//
// 规则(够用即可,不做通用 diff3):
// - 一侧没动 → 取另一侧;
// - remote 相对 base 改掉的行(按出现顺序与新增的行配对)→ 在 local 里找到同一行原地替换,
//   local 里已没有这行(用户也改/删了)→ 新行追加到末尾,不丢字;
// - remote 多出来的行 → 追加到 local 末尾(快捷键追加的就是这种);
// - remote 删掉的行 → local 里还原样在就删掉。
// 空行不参与比对;结果用空行分隔追加的块。

function bag(lines: readonly string[]): Map<string, number> {
  const m = new Map<string, number>();
  for (const l of lines) m.set(l, (m.get(l) ?? 0) + 1);
  return m;
}

/** a 中比 b 多出来的行(多重集合差,保持 a 的顺序)。 */
function minus(a: readonly string[], b: readonly string[]): string[] {
  const left = bag(b);
  const out: string[] = [];
  for (const l of a) {
    const n = left.get(l) ?? 0;
    if (n > 0) left.set(l, n - 1);
    else out.push(l);
  }
  return out;
}

const content = (s: string) => s.split("\n").filter((l) => l.trim() !== "");

export function mergeMemoDoc(base: string, local: string, remote: string): string {
  if (local === base) return remote;
  if (remote === base || remote === local) return local;
  const b = content(base);
  const r = content(remote);
  const added = minus(r, b);
  const removed = minus(b, r);
  const lines = local.replace(/\s+$/, "").split("\n");
  const appended: string[] = [];
  const pairs = Math.min(added.length, removed.length);
  for (let i = 0; i < pairs; i++) {
    const at = lines.indexOf(removed[i]);
    if (at >= 0) lines[at] = added[i];
    else appended.push(added[i]);
  }
  for (const a of added.slice(pairs)) appended.push(a);
  for (const gone of removed.slice(pairs)) {
    const at = lines.indexOf(gone);
    if (at >= 0) lines.splice(at, 1);
  }
  let out = lines.join("\n").replace(/\n{3,}/g, "\n\n").replace(/^\n+/, "");
  for (const a of appended) {
    // 已经在 local 里的(两边恰好写了同一行)不重复追加
    if (lines.includes(a)) continue;
    out = out.trim() === "" ? a : `${out.replace(/\s+$/, "")}\n\n${a}`;
  }
  return out.trim() === "" ? "" : `${out.replace(/\s+$/, "")}\n`;
}
