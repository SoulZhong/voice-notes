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
      const routes = family === "classic" ? ROUTES : ["/settings", `/notes/${NOTE}`, "/record"];
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

  // 4. 笔记页:书签显示、备注栏列出、按 M 加备注
  await scenario("笔记页备注:书签+备注栏+按 M 加一条", async () => {
    const { page, errors } = await openPage(`/notes/${NOTE}`, { family: "classic", mode: "dark" });
    assert(await waitFor(() => hasText(page, "定了:周二上线")), "备注文字没出现在页面上");
    const bookmark = await page.$(".ProseMirror .memo-bookmark, .ProseMirror [class*='memo']");
    assert(bookmark, "编辑器里没有书签装饰");
    await page.mouse.click(5, 700);
    await page.keyboard.press("m");
    const draft = await waitFor(() => page.$(".ProseMirror textarea, .memo-draft textarea, textarea"));
    assert(draft, "按 M 没出现草稿输入框");
    await draft.type("e2e 补一条");
    await page.keyboard.press("Enter");
    const added = await waitFor(async () => (await calls(page, "add_memo")).find((c) => c.args.origin === "note"));
    assert(added, "回车后没调用 add_memo(origin=note)");
    assert(await waitFor(() => hasText(page, "e2e 补一条")), "新备注没渲染出来");
    await page.screenshot({ path: join(out, "flow-note-memo.png") });
    const bad = errors.filter((e) => !benign(e));
    assert(bad.length === 0, "页面错误:\n" + bad.join("\n"));
    await page.close();
  });

  // 5. 录制页:输入框首次按键取锚点,回车以锚点时间保存
  await scenario("录制页备注:开始打字取锚点,回车保存", async () => {
    const { page, errors } = await openPage("/record", { family: "paper", mode: "light", recording: true });
    const ta = await waitFor(() => page.$("textarea"));
    assert(ta, "录制中没有备注输入框");
    await ta.type("现场记一句");
    const anchored = await waitFor(async () => (await calls(page, "memo_anchor_now")).length > 0);
    assert(anchored, "开始打字没有调用 memo_anchor_now");
    await page.keyboard.press("Enter");
    const added = await waitFor(async () => (await calls(page, "add_memo")).find((c) => c.args.origin === "composer"));
    assert(added && added.args.atMs === 7000, `add_memo 的时间点应为锚点 7000,实际 ${added && added.args.atMs}`);
    assert(await waitFor(() => hasText(page, "现场记一句")), "新备注没出现在书签/备注栏");
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
    const edited = await waitFor(async () => (await calls(page, "edit_memo")).find((c) => c.args.text === "浮窗补一句"));
    assert(edited, "回车没调用 edit_memo");
    assert(await waitFor(async () => (await calls(page, "memo_panel_hide")).length > 0), "保存后没关闭浮窗");
    const bad = errors.filter((e) => !benign(e));
    assert(bad.length === 0, "页面错误:\n" + bad.join("\n"));
    await page.screenshot({ path: join(out, "flow-memo-panel.png") });
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
