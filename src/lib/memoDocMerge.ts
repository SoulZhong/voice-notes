// 备注文档的三方合并(按逻辑行做 diff3)。与后端 store/memos.rs 的 merge3 同一算法:后端在
// 保存时用它把编辑器的改动合到盘面上;前端用它把保存期间新打的字接到保存回执上、把别处的
// 新版本接到本地未存的改动上。
//
// base = local 所依据的版本,remote = 另一侧(盘上)现状。
// - 三方都对得上的行是锚点;锚点之间的区间:一侧没动取另一侧;两侧一样取其一;行数都相同
//   逐行按位置定;一侧只是插了行就取另一侧的改动再把插入的行放回;其余两边都留,不丢字。
// - 按位置对齐,不按内容找行:同一时间点的两段文字可以一样,正文不能当段落身份(Codex 审出)。
// - 比对单位是「逻辑行」:行尾 `\` 的硬换行把下一行连进来,多行段落整体进出。

function units(s: string): string[] {
  const out: string[] = [];
  for (const line of s.replace(/\n+$/, "").split("\n")) {
    const last = out.length - 1;
    if (last >= 0 && out[last].endsWith("\\")) out[last] += `\n${line}`;
    else out.push(line);
  }
  return out;
}

function lcsPairs(a: readonly string[], b: readonly string[]): Map<number, number> {
  const n = a.length;
  const m = b.length;
  const dp: Uint32Array[] = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--)
    for (let j = m - 1; j >= 0; j--)
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
  const out = new Map<number, number>();
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      out.set(i++, j++);
    } else if (dp[i + 1][j] >= dp[i][j + 1]) i++;
    else j++;
  }
  return out;
}

const eq = (x: readonly string[], y: readonly string[]) => x.length === y.length && x.every((v, i) => v === y[i]);

function onlyInserted(o: readonly string[], x: readonly string[]): [string[], string[]] | null {
  let k = 0;
  const before: string[] = [];
  const rest: string[] = [];
  for (const l of x) {
    if (k < o.length && l === o[k]) k++;
    else if (k === 0) before.push(l);
    else rest.push(l);
  }
  return k === o.length ? [before, rest] : null;
}

function pushExtra(out: string[], extra: readonly string[], have: readonly string[]) {
  const add = extra.filter((l) => l.trim() !== "" && !have.includes(l));
  if (add.length === 0) return;
  const last = out[out.length - 1];
  if (last !== undefined && last.trim() !== "") out.push("");
  out.push(...add);
}

function resolve(o: string[], a: string[], b: string[], out: string[]) {
  if (eq(a, o)) out.push(...b);
  else if (eq(b, o) || eq(a, b)) out.push(...a);
  else if (o.length > 0 && a.length === o.length && b.length === o.length) {
    for (let k = 0; k < o.length; k++) {
      if (a[k] === o[k]) out.push(b[k]);
      else if (b[k] === o[k] || a[k] === b[k]) out.push(a[k]);
      else out.push(a[k], "", b[k]);
    }
  } else if (o.length === 0) {
    out.push(...a);
    pushExtra(out, b, a);
  } else {
    const ia = onlyInserted(o, a);
    if (ia) {
      out.push(...ia[0], ...b);
      pushExtra(out, ia[1], b);
      return;
    }
    const ib = onlyInserted(o, b);
    if (ib) {
      const start = out.length;
      out.push(...ib[0].filter((l) => !a.includes(l)));
      if (out.length > start && a[0] !== undefined && a[0].trim() !== "") out.push("");
      out.push(...a);
      pushExtra(out, ib[1], a);
      return;
    }
    out.push(...a);
    pushExtra(out, b, a);
  }
}

export function mergeMemoDoc(base: string, local: string, remote: string): string {
  if (local === base) return remote;
  if (remote === base || remote === local) return local;
  const o = units(base);
  const a = units(local);
  const b = units(remote);
  const ma = lcsPairs(o, a);
  const mb = lcsPairs(o, b);
  const out: string[] = [];
  let oi = 0;
  let ai = 0;
  let bi = 0;
  for (;;) {
    let k = oi;
    for (; k < o.length; k++) {
      const x = ma.get(k);
      const y = mb.get(k);
      if (x !== undefined && y !== undefined && x >= ai && y >= bi) break;
    }
    if (k >= o.length) {
      resolve(o.slice(oi), a.slice(ai), b.slice(bi), out);
      break;
    }
    const x = ma.get(k)!;
    const y = mb.get(k)!;
    resolve(o.slice(oi, k), a.slice(ai, x), b.slice(bi, y), out);
    out.push(o[k]);
    oi = k + 1;
    ai = x + 1;
    bi = y + 1;
  }
  const text = out.join("\n").replace(/\n{3,}/g, "\n\n").replace(/^\n+|\n+$/g, "");
  return text.trim() === "" ? "" : `${text}\n`;
}
