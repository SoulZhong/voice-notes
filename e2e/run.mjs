// 前端端到端测试(UI 改版 #730 / 会议备注 #729):起 vite dev,用本机 chrome-headless-shell
// 驱动真实页面;后端由 e2e/mock-tauri.js 模拟(命令打桩 + 事件推送)。
// 用法:npm run e2e   (可选 E2E_CHROME=<chrome 路径>、E2E_PORT=1431)
// 产出:e2e/out/*.png 截图;任何场景失败 → 退出码 1。
import { spawn } from "node:child_process";
import { readFileSync, mkdirSync, readdirSync, existsSync } from "node:fs";
import { homedir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import puppeteer from "puppeteer-core";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const out = join(here, "out");
mkdirSync(out, { recursive: true });
const PORT = Number(process.env.E2E_PORT || 1431);
const BASE = `http://localhost:${PORT}`;
const MOCK = readFileSync(join(here, "mock-tauri.js"), "utf8");
const NOTE = "20261104-100000";

function findChrome() {
  if (process.env.E2E_CHROME) return process.env.E2E_CHROME;
  const dir = join(homedir(), ".cache/puppeteer/chrome-headless-shell");
  const vers = existsSync(dir) ? readdirSync(dir).sort() : [];
  for (const v of vers.reverse()) {
    const p = join(dir, v, "chrome-headless-shell-mac-arm64/chrome-headless-shell");
    if (existsSync(p)) return p;
  }
  throw new Error("找不到 chrome-headless-shell;设置 E2E_CHROME");
}

async function startVite() {
  const p = spawn("npx", ["vite", "dev", "--port", String(PORT), "--strictPort"], { cwd: root, stdio: ["ignore", "pipe", "pipe"] });
  let log = "";
  p.stdout.on("data", (d) => (log += d));
  p.stderr.on("data", (d) => (log += d));
  for (let i = 0; i < 120; i++) {
    try {
      const r = await fetch(`${BASE}/`);
      if (r.ok) return p;
    } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  p.kill();
  throw new Error("vite 没起来:\n" + log);
}

const results = [];
let browser;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function openPage(path, params = {}, { width = 1100, height = 760 } = {}) {
  const page = await browser.newPage();
  await page.setViewport({ width, height });
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  page.on("console", (m) => {
    if (m.type() === "error") errors.push(m.text());
  });
  await page.evaluateOnNewDocument(`window.__E2E_PARAMS__ = ${JSON.stringify(params)};`);
  await page.evaluateOnNewDocument(MOCK);
  if (params.cache) {
    await page.evaluateOnNewDocument(`try { localStorage.setItem("vn.appearance", ${JSON.stringify(JSON.stringify(params.cache))}); } catch {}`);
  }
  await page.evaluateOnNewDocument(`document.addEventListener("DOMContentLoaded", () => { window.__firstTheme = document.documentElement.dataset.vnTheme; });`);
  await page.goto(BASE + path, { waitUntil: "networkidle0", timeout: 60000 });
  await new Promise((r) => setTimeout(r, 400));
  return { page, errors };
}

const calls = (page, cmd) => page.evaluate((c) => window.__e2e.calls.filter((x) => x.cmd === c), cmd);
const waitFor = async (fn, ms = 5000) => {
  const t0 = Date.now();
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() - t0 > ms) return v;
    await new Promise((r) => setTimeout(r, 100));
  }
};
const hasText = (page, text) => page.evaluate((t) => document.body.innerText.includes(t), text);
// 已知可忽略的控制台噪音(与被测功能无关):资源 404、开发态 HMR 提示等。
const benign = (e) => /favicon|Failed to load resource|\[vite\]|posthog|\[TAURI\] Couldn't find callback/i.test(e);

async function scenario(name, fn) {
  const t0 = Date.now();
  try {
    await fn();
    results.push({ name, ok: true, ms: Date.now() - t0 });
    console.log(`✔ ${name}`);
  } catch (e) {
    results.push({ name, ok: false, err: String(e?.stack || e) });
    console.log(`✘ ${name}\n   ${String(e?.message || e)}`);
  }
}
function assert(c, msg) {
  if (!c) throw new Error(msg);
}

const ROUTES = ["/settings", `/notes/${NOTE}`, "/speakers", "/ai", "/graph", "/hooks", "/record", "/device"];

const vite = await startVite();
browser = await puppeteer.launch({ executablePath: findChrome(), headless: true, args: ["--no-sandbox", "--hide-scrollbars"] });
try {
  // 1. 冒烟:各页面在三套皮肤 × 亮暗下都能渲染,无脚本错误
  for (const family of ["classic", "paper", "nord"]) {
    for (const mode of ["light", "dark"]) {
      const routes = ROUTES;
      for (const r of routes) {
        await scenario(`渲染 ${r} · ${family}/${mode}`, async () => {
          const { page, errors } = await openPage(r, { family, mode });
          const theme = await page.evaluate(() => document.documentElement.dataset.vnTheme);
          const m = await page.evaluate(() => document.documentElement.dataset.vnMode);
          assert(theme === family, `data-vn-theme=${theme},期望 ${family}`);
          assert(m === mode, `data-vn-mode=${m},期望 ${mode}`);
          const sidebar = await page.$("aside, nav");
          assert(sidebar, "侧栏没渲染");
          const bad = errors.filter((e) => !benign(e));
          await page.screenshot({ path: join(out, `${r.replace(/\W+/g, "_")}-${family}-${mode}.png`) });
          assert(bad.length === 0, "控制台/页面错误:\n" + bad.join("\n"));
          await page.close();
        });
      }
    }
  }

  // 2. 首帧:缓存里的皮肤在应用脚本跑之前就生效(不闪默认色)
  await scenario("首帧外观:先用缓存皮肤,再按后端设置对账", async () => {
    const { page } = await openPage("/settings", { family: "classic", mode: "light", cache: { family: "nord", mode: "dark" } });
    const first = await page.evaluate(() => window.__firstTheme);
    const now = await page.evaluate(() => document.documentElement.dataset.vnTheme);
    assert(first === "nord", `首帧 data-vn-theme=${first},期望 nord(来自缓存)`);
    assert(now === "classic", `对账后 data-vn-theme=${now},期望 classic(来自后端设置)`);
    await page.close();
  });

  // 3. 设置页切皮肤:立即生效 + 落盘
  await scenario("设置页切换皮肤为纸张", async () => {
    const { page } = await openPage("/settings", { family: "classic", mode: "light" });
    const radios = await page.$$('[role="radio"]');
    let clicked = false;
    for (const r of radios) {
      const txt = await r.evaluate((el) => el.textContent || "");
      if (txt.includes("纸张")) {
        await r.click();
        clicked = true;
        break;
      }
    }
    assert(clicked, "没找到「纸张」皮肤卡");
    const ok = await waitFor(() => page.evaluate(() => document.documentElement.dataset.vnTheme === "paper"));
    assert(ok, "点了纸张,根元素皮肤没变");
    const saved = await calls(page, "set_settings");
    assert(saved.some((c) => JSON.stringify(c.args).includes('"theme_family":"paper"')), "没有以 theme_family=paper 落盘");
    await page.screenshot({ path: join(out, "flow-settings-paper.png") });
    await page.close();
  });

  // 4. 笔记页:备注是一篇 Markdown;按 M 在文末开一行写,自动保存整篇
  await scenario("笔记页备注:Markdown 文档+按 M 写一行+自动保存", async () => {
    const { page, errors } = await openPage(`/notes/${NOTE}`, { family: "classic", mode: "dark" });
    assert(await waitFor(() => page.evaluate(() => document.querySelector(".memo-doc .ProseMirror")?.textContent.includes("定了:周二上线"))), "备注文档没载入");
    assert(await waitFor(() => page.evaluate(() => document.querySelector(".memo-doc .memo-stamp")?.textContent === "00:06")), "时间点没显示在左侧槽");
    assert(!(await page.$(".transcript .memo-bm")), "原文里不该再有备注书签");
    await page.mouse.click(5, 700);
    await page.keyboard.press("m");
    await sleep(150);
    await page.keyboard.type("## 决议");
    await page.keyboard.press("Enter");
    await page.keyboard.type("- 周二上线");
    const saved = await waitFor(async () => (await calls(page, "save_memo_doc")).find((c) => c.args.text.includes("- 周二上线 <!--")), 4000);
    assert(saved, "停手后没整篇保存(或没带时间点)");
    const txt = saved.args.text;
    assert(/## 决议 <!-- \d\d:\d\d -->/.test(txt), `标题没带时间点:${txt}`);
    assert(txt.startsWith("定了:周二上线 <!-- 00:06 -->"), `原有内容被改动:${txt}`);
    await page.screenshot({ path: join(out, "flow-note-memo.png") });
    const bad = errors.filter((e) => !benign(e));
    assert(bad.length === 0, "页面错误:\n" + bad.join("\n"));
    await page.close();
  });

  // 5. 录制页:右侧备注文档,写一行带录音时间;快捷键追加的行合并进来不丢本地输入
  await scenario("录制页备注:写一行带录音时间,快捷键追加合并", async () => {
    const { page, errors } = await openPage("/record", { family: "paper", mode: "light", recording: true });
    const ed = await waitFor(() => page.$(".memo-doc .ProseMirror"));
    assert(ed, "录制中没有备注文档");
    assert(!(await page.$(".memo-composer, .live-body textarea")), "不该再有底部输入框");
    await waitFor(() => page.evaluate(() => document.querySelector(".memo-doc .ProseMirror")?.textContent.includes("定了")));
    // 窄窗口默认收起成细轨:先展开
    if (await page.$(".memo-doc.collapsed")) await page.click(".memo-doc .track .toggle");
    // 光标放到文末(点击位置在无头浏览器里不稳定,直接设选区)
    await page.evaluate(() => {
      const pm = document.querySelector(".memo-doc .ProseMirror");
      pm.focus();
      const r = document.createRange();
      r.selectNodeContents(pm.lastElementChild);
      r.collapse(false);
      getSelection().removeAllRanges();
      getSelection().addRange(r);
    });
    await sleep(150); // 等 PM 读到 selectionchange
    await page.keyboard.press("Enter");
    await page.keyboard.type("现场记一句");
    const saved = await waitFor(async () => (await calls(page, "save_memo_doc")).find((c) => c.args.text.includes("现场记一句 <!--")), 4000);
    assert(saved, `没有保存带时间点的新行:${JSON.stringify(await page.evaluate(() => [document.querySelector(".memo-doc .ProseMirror")?.innerText, window.__e2e.calls.filter((c) => c.cmd.includes("memo")).map((c) => c.cmd)]))}`);
    assert(/^定了:周二上线 <!-- 00:06 -->\n\n现场记一句 <!-- 00:1\d -->\n$/.test(saved.args.text), `新行应带录音时间(约 15 秒):${saved.args.text}`);
    // 改成标题(内置快捷键只传 level):时间点要沿用,不能被重打成「现在」
    const stampBefore = /现场记一句 <!-- (\d\d:\d\d) -->/.exec(saved.args.text)[1];
    await sleep(2200);
    await page.keyboard.down("Meta");
    await page.keyboard.down("Alt");
    await page.keyboard.press("Digit2");
    await page.keyboard.up("Alt");
    await page.keyboard.up("Meta");
    const asHeading = await waitFor(async () => (await calls(page, "save_memo_doc")).find((c) => c.args.text.includes("## 现场记一句")), 4000);
    assert(asHeading, "改成标题后没保存");
    assert(asHeading.args.text.includes(`## 现场记一句 <!-- ${stampBefore} -->`), `改成标题后时间点变了:${asHeading.args.text}`);
    // 模拟快捷键在别处追加了一行(盘上变了),此时本地又改了一处 → 合并后两边都在
    await page.keyboard.type("再补");
    await page.evaluate(async () => {
      const inv = window.__TAURI_INTERNALS__.invoke;
      await inv("edit_memo_line", { noteId: window.__e2e.NOTE_ID, atMs: 15000, oldText: "不存在", text: "快捷键那句" });
    });
    const merged = await waitFor(async () => {
      const t = await page.evaluate(() => window.__e2e.memoText);
      return t.includes("快捷键那句") && t.includes("现场记一句再补") ? t : null;
    }, 5000);
    assert(merged, `合并后应同时保留快捷键追加与本地输入:${await page.evaluate(() => window.__e2e.memoText)}`);
    const overlap = await page.evaluate(() => {
      const col = document.querySelector(".memo-doc header")?.getBoundingClientRect();
      const bar = document.querySelector(".topbar")?.getBoundingClientRect();
      return col && bar ? col.top < bar.bottom + 14 - 1 : "missing";
    });
    assert(overlap === false, `备注栏标题被吸顶操作栏(含渐隐)盖住或缺失:${overlap}`);
    await page.screenshot({ path: join(out, "flow-record-memo.png") });
    const bad = errors.filter((e) => !benign(e));
    assert(bad.length === 0, "页面错误:\n" + bad.join("\n"));
    await page.close();
  });

  // 6. 侧栏搜索能搜到备注
  await scenario("侧栏搜索命中备注", async () => {
    const { page } = await openPage(`/notes/${NOTE}`, { family: "nord", mode: "dark" });
    const input = await page.$('aside input[type="search"], aside input');
    assert(input, "侧栏没有搜索框");
    await input.type("周二");
    const searched = await waitFor(async () => (await calls(page, "search_memos")).length > 0, 3000);
    assert(searched, "输入后没有调用 search_memos");
    assert(await waitFor(() => page.evaluate(() => !!document.querySelector(".memo-hit") || document.body.innerText.includes("定了:周二上线"))), "命中行没露出备注");
    await page.close();
  });

  // 7. 备注浮窗:显示钉住时间,回车保存并关闭
  await scenario("备注浮窗:补文字回车保存并关闭", async () => {
    const { page, errors } = await openPage("/memo-panel", { family: "classic", mode: "dark" }, { width: 440, height: 150 });
    assert(await waitFor(() => hasText(page, "00:06")), "浮窗没显示钉住时间 00:06");
    const ta = await page.$("textarea");
    await ta.type("浮窗补一句");
    await page.keyboard.press("Enter");
    const edited = await waitFor(async () => (await calls(page, "edit_memo_line")).find((c) => c.args.text === "浮窗补一句" && c.args.atMs === 6000));
    assert(edited, "回车没调用 edit_memo_line(时间点 6000)");
    assert(await waitFor(async () => (await calls(page, "memo_panel_hide")).length > 0), "保存后没关闭浮窗");
    const bad = errors.filter((e) => !benign(e));
    assert(bad.length === 0, "页面错误:\n" + bad.join("\n"));
    await page.screenshot({ path: join(out, "flow-memo-panel.png") });
    await page.close();
  });
  // 8. Codex 复审回归:单选分段再点已选项,不能变成「无选中」(bits 先改内部值再回调)
  await scenario("单选分段:再点已选项仍保持选中", async () => {
    const { page, errors } = await openPage("/ai/logs", { family: "classic", mode: "light" });
    const on = () => page.$$eval('[data-slot="toggle-group-item"][data-state="on"]', (els) => els.length);
    assert((await on()) === 1, "初始应恰有一项选中");
    await page.click('[data-slot="toggle-group-item"][data-state="on"]');
    await new Promise((r) => setTimeout(r, 200));
    assert((await on()) === 1, "再点已选项后变成了无选中");
    const bad = errors.filter((e) => !benign(e));
    assert(bad.length === 0, "页面错误:\n" + bad.join("\n"));
    await page.close();
  });

  // 9. Codex 复审回归:设置页开关切换一次只保存一次,且值正确
  await scenario("设置页开关:切一次只落盘一次", async () => {
    const { page } = await openPage("/settings", { family: "nord", mode: "dark" });
    const before = (await calls(page, "set_settings")).length;
    const sw = await page.$$('[data-slot="switch"]');
    assert(sw.length > 3, "设置页开关太少,疑似没渲染");
    // 「菜单栏常驻」走 set_settings 落盘(自启/统计等走各自命令,不在此断言范围)
    const handle = await page.evaluateHandle(() => {
      const row = [...document.querySelectorAll(".row")].find((r) => r.textContent?.includes("菜单栏常驻"));
      return row?.querySelector('[data-slot="switch"]') ?? null;
    });
    const target = handle.asElement();
    assert(target, "没找到「菜单栏常驻」开关");
    const was = await target.evaluate((el) => el.getAttribute("data-state"));
    await target.click();
    await new Promise((r) => setTimeout(r, 300));
    const after = await calls(page, "set_settings");
    assert(after.length - before === 1, `切一次开关落盘了 ${after.length - before} 次`);
    assert(JSON.stringify(after.at(-1).args).includes(`"tray_enabled":${was !== "checked"}`), "落盘的 tray_enabled 值不对");
    const now = await target.evaluate((el) => el.getAttribute("data-state"));
    assert(now !== was, "开关状态没变");
    await page.close();
  });

  // 10. 录制中说话人编辑:删除说话人后,条上的 chip 消失、已上屏段落清掉归属(cleared)
  await scenario("录制页说话人条:删除说话人并清掉已上屏段归属", async () => {
    const { page, errors } = await openPage("/record", { family: "classic", mode: "light", recording: true });
    assert(await waitFor(() => hasText(page, "李四")), "录制页没水合出说话人「李四」");
    const chip = await page.evaluateHandle(
      () => [...document.querySelectorAll(".chip button.name")].find((b) => b.textContent?.trim() === "李四") ?? null,
    );
    assert(chip.asElement(), "说话人条上没有「李四」chip");
    await chip.asElement().click();
    const clickRow = (text) =>
      page.evaluate((t) => {
        const b = [...document.querySelectorAll(".chip .panel button, .chip .panel a")].find((x) => x.textContent?.includes(t));
        b?.click();
        return !!b;
      }, text);
    assert(await waitFor(() => clickRow("删除这个说话人")), "编辑面板里没有「删除这个说话人」");
    assert(await waitFor(() => clickRow("确认删除")), "没出现删除二段确认");
    const del = await waitFor(async () => (await calls(page, "delete_note_speaker")).find((c) => c.args.speakerId === "S2"));
    assert(del, "确认后没调用 delete_note_speaker(S2)");
    assert(await waitFor(async () => !(await hasText(page, "李四"))), "删除后页面上仍有「李四」(chip 或段落徽章没清)");
    // 不靠 cleared 清归属的话,这段会显示成「新说话人 2」(表里没了该 id 的兜底名)
    const badge = await page.$eval("#seg-1 .badge", (el) => el.textContent?.trim() ?? "");
    assert(badge === "对方", `被删说话人的段落徽章应回到未标注「对方」,实际「${badge}」`);
    const bad = errors.filter((e) => !benign(e));
    assert(bad.length === 0, "页面错误:\n" + bad.join("\n"));
    await page.screenshot({ path: join(out, "flow-record-speaker-delete.png") });
    await page.close();
  });

  // 11. 笔记页「更多」菜单:按下→停一会儿→松开(真人点击的节奏)后菜单仍开着。
  // 模态菜单按下即开并给 body 加 pointer-events:none,松手的 click 落到 <html>;
  // 若 window 的 click 关闭处理也管这个菜单,就会「点开就消失」。
  await scenario("笔记页更多菜单:慢点击后保持打开", async () => {
    const { page, errors } = await openPage(`/notes/${NOTE}`, { family: "classic", mode: "light" });
    const btn = await waitFor(() => page.$('button[aria-label="更多"]'));
    assert(btn, "笔记页没有「更多」按钮");
    const box = await btn.boundingBox();
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.mouse.down();
    await sleep(150);
    await page.mouse.up();
    await sleep(400);
    const open = await page.evaluate(() => !!document.querySelector('[data-slot="dropdown-menu-content"][data-state="open"]'));
    assert(open, "松开鼠标后「更多」菜单自己关掉了");
    await page.keyboard.press("Escape");
    assert(
      await waitFor(() => page.evaluate(() => !document.querySelector('[data-slot="dropdown-menu-content"]'))),
      "Esc 没关掉「更多」菜单",
    );
    const bad = errors.filter((e) => !benign(e));
    assert(bad.length === 0, "页面错误:\n" + bad.join("\n"));
    await page.close();
  });
} finally {
  await browser.close();
  vite.kill();
}

const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} 通过;截图在 e2e/out/`);
if (failed.length) {
  for (const f of failed) console.log(`\n--- ${f.name}\n${f.err}`);
  process.exit(1);
}
