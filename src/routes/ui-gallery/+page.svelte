<script lang="ts">
  // 主题 × 组件预览台(开发用,文案不进字典)。URL 参数 ?family=paper&mode=dark 直达某一组合,
  // 便于无头浏览器逐组截图对比;页内按钮可现场切换。
  import { onMount } from "svelte";
  import { page } from "$app/stores";
  import { applyTheme, THEME_FAMILIES, COLOR_MODES, normalizeFamily, normalizeMode } from "$lib/theme";
  import { Button } from "$lib/ui/components/button";
  import { Input } from "$lib/ui/components/input";
  import { Textarea } from "$lib/ui/components/textarea";
  import { Switch } from "$lib/ui/components/switch";
  import { Badge } from "$lib/ui/components/badge";
  import { Separator } from "$lib/ui/components/separator";
  import * as Select from "$lib/ui/components/select";
  import * as Tabs from "$lib/ui/components/tabs";
  import * as DropdownMenu from "$lib/ui/components/dropdown-menu";
  import * as Popover from "$lib/ui/components/popover";
  import * as Dialog from "$lib/ui/components/dialog";

  let family = $state(normalizeFamily($page.url.searchParams.get("family")));
  let mode = $state(normalizeMode($page.url.searchParams.get("mode") ?? "dark"));
  let engine = $state("sense_voice");
  let on = $state(true);
  let openAll = $page.url.searchParams.get("open") === "1";

  $effect(() => applyTheme(mode, family));
  onMount(() => applyTheme(mode, family));

  const speakers = [
    ["tint-sky", "Alice"],
    ["tint-mint", "Bob"],
    ["tint-lavender", "Chen"],
    ["tint-rose", "Dana"],
    ["tint-yellow", "Eve"],
  ];
</script>

<div class="gallery">
  <header class="bar">
    <strong>UI gallery</strong>
    <span class="grp">
      {#each THEME_FAMILIES as f (f)}
        <Button size="sm" variant={family === f ? "default" : "outline"} onclick={() => (family = f)}>{f}</Button>
      {/each}
    </span>
    <span class="grp">
      {#each COLOR_MODES as m (m)}
        <Button size="sm" variant={mode === m ? "secondary" : "ghost"} onclick={() => (mode = m)}>{m}</Button>
      {/each}
    </span>
  </header>

  <section>
    <h2>Buttons</h2>
    <div class="rowx">
      <Button>Start recording</Button>
      <Button variant="secondary">Secondary</Button>
      <Button variant="outline">Outline</Button>
      <Button variant="ghost">Ghost</Button>
      <Button variant="destructive">Delete</Button>
      <Button variant="link">Link</Button>
      <Button disabled>Disabled</Button>
    </div>
  </section>

  <section>
    <h2>Form</h2>
    <div class="form">
      <Input placeholder="Filter by title" />
      <Select.Root type="single" bind:value={engine}>
        <Select.Trigger class="w-48">{engine}</Select.Trigger>
        <Select.Content>
          <Select.Item value="sense_voice">SenseVoice</Select.Item>
          <Select.Item value="apple">System (Apple)</Select.Item>
          <Select.Item value="qwen3">Qwen3-ASR</Select.Item>
        </Select.Content>
      </Select.Root>
      <label class="sw"><Switch bind:checked={on} /> Global shortcut</label>
      <Textarea placeholder="Jot a memo… (Enter to save)" rows={2} />
    </div>
  </section>

  <section>
    <h2>Overlays</h2>
    <div class="rowx">
      <DropdownMenu.Root open={openAll}>
        <DropdownMenu.Trigger>{#snippet child({ props })}<Button {...props} variant="outline">Note menu</Button>{/snippet}</DropdownMenu.Trigger>
        <DropdownMenu.Content>
          <DropdownMenu.Item>Rename</DropdownMenu.Item>
          <DropdownMenu.Item>Export Markdown</DropdownMenu.Item>
          <DropdownMenu.Separator />
          <DropdownMenu.Item variant="destructive">Delete note</DropdownMenu.Item>
        </DropdownMenu.Content>
      </DropdownMenu.Root>
      <Popover.Root>
        <Popover.Trigger>{#snippet child({ props })}<Button {...props} variant="outline">Speaker</Button>{/snippet}</Popover.Trigger>
        <Popover.Content class="w-64">
          <Input placeholder="Search people" />
          <ul class="plist"><li>Alice</li><li>Bob</li><li>Chen</li></ul>
        </Popover.Content>
      </Popover.Root>
      <Dialog.Root>
        <Dialog.Trigger>{#snippet child({ props })}<Button {...props} variant="outline">Dialog</Button>{/snippet}</Dialog.Trigger>
        <Dialog.Content>
          <Dialog.Header>
            <Dialog.Title>Delete this note?</Dialog.Title>
            <Dialog.Description>The transcript and audio will be removed.</Dialog.Description>
          </Dialog.Header>
          <Dialog.Footer>
            <Button variant="outline">Cancel</Button>
            <Button variant="destructive">Delete</Button>
          </Dialog.Footer>
        </Dialog.Content>
      </Dialog.Root>
    </div>
  </section>

  <section>
    <h2>Tabs · Badges</h2>
    <Tabs.Root value="raw">
      <Tabs.List>
        <Tabs.Trigger value="raw">Raw</Tabs.Trigger>
        <Tabs.Trigger value="refined">Refined</Tabs.Trigger>
        <Tabs.Trigger value="graph">Graph</Tabs.Trigger>
      </Tabs.List>
    </Tabs.Root>
    <div class="rowx">
      <Badge>Default</Badge>
      <Badge variant="secondary">Dictation</Badge>
      <Badge variant="outline">Interrupted</Badge>
      <Badge variant="destructive">Error</Badge>
    </div>
  </section>

  <Separator />

  <section>
    <h2>App surfaces (existing tokens)</h2>
    <div class="transcript">
      {#each speakers as [tint, name], i (name)}
        <p>
          <span class="spk" style="background: var(--{tint}); color: var(--{tint}-ink)">{name}</span>
          <span class="ts">00:0{i}:12</span>
          We agreed to ship next Tuesday and freeze on Monday; <a href="#x">link</a>.
        </p>
      {/each}
      <div class="memo">Memo 12:34 · Ship Tuesday</div>
      <p class="rec"><span class="dot"></span> Recording 00:42:10 · <span class="faint">faint timestamp text</span></p>
    </div>
  </section>
</div>

<style>
  .gallery {
    min-height: 100vh;
    background: var(--canvas);
    color: var(--ink);
    padding: 1rem 1.25rem 3rem;
    font-family: -apple-system, BlinkMacSystemFont, "PingFang SC", sans-serif;
    font-size: 14px;
  }
  .bar {
    display: flex;
    align-items: center;
    gap: 1rem;
    flex-wrap: wrap;
    margin-bottom: 0.5rem;
  }
  .grp {
    display: inline-flex;
    gap: 0.3rem;
  }
  h2 {
    font-size: 0.85rem;
    font-weight: 600;
    color: var(--ink-secondary);
    margin: 1.2rem 0 0.5rem;
  }
  .rowx {
    display: flex;
    gap: 0.5rem;
    flex-wrap: wrap;
    align-items: center;
    margin: 0.4rem 0;
  }
  .form {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 0.6rem;
    max-width: 640px;
    align-items: center;
  }
  .sw {
    display: inline-flex;
    gap: 0.5rem;
    align-items: center;
  }
  .plist {
    list-style: none;
    padding: 0;
    margin: 0.5rem 0 0;
  }
  .plist li {
    padding: 0.3rem 0.4rem;
    border-radius: var(--radius-md);
  }
  .plist li:hover {
    background: var(--surface-press);
  }
  .transcript {
    background: var(--surface);
    border-radius: var(--radius-xl);
    padding: 16px 20px;
    line-height: 1.7;
    max-width: 720px;
  }
  .transcript p {
    margin: 0 0 6px;
  }
  .spk {
    display: inline-block;
    padding: 0 0.5em;
    border-radius: var(--radius-sm);
    font-size: 0.85em;
    margin-right: 0.4em;
  }
  .ts {
    color: var(--ink-faint);
    font-variant-numeric: tabular-nums;
    margin-right: 0.4em;
    font-size: 0.85em;
  }
  a {
    color: var(--accent);
  }
  .memo {
    margin: 8px 0 10px -12px;
    padding: 0.4rem 0.7rem;
    border-left: 2px solid var(--accent);
    background: var(--accent-tint);
    border-radius: 0 var(--radius-md) var(--radius-md) 0;
  }
  .rec {
    color: var(--ink-secondary);
  }
  .dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--record);
  }
  .faint {
    color: var(--ink-faint);
  }
</style>
