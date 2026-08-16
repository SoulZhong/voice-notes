<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { listNotes } from "$lib/notes";
  import { t } from "$lib/i18n/index.svelte";

  let empty = $state(false);

  onMount(async () => {
    try {
      const notes = await listNotes();
      // 落地重定向只在"还停在根路由"时才做(Codex P2):listNotes 是异步的,等它的这段时间里
      // 用户/托盘可能已经把页面导走了(冷启动点托盘「打开设置」正是如此)——组件虽已销毁,
      // 这个 promise 仍会跑完,不设防就会把人从设置页又踢回 /record。
      if (window.location.pathname !== "/") return;
      if (notes.length > 0) {
        if (notes[0].state === "active") {
          goto("/record", { replaceState: true });
        } else {
          goto(`/notes/${notes[0].id}`, { replaceState: true });
        }
      } else {
        empty = true;
      }
    } catch {
      empty = true;
    }
  });
</script>

{#if empty}
  <div class="empty">
    <p>{t("shell.home.empty")}</p>
    <p class="hint">{t("shell.home.emptyHint")}</p>
  </div>
{/if}

<style>
  .empty {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    color: var(--ink-secondary);
  }
  .hint {
    color: var(--ink-faint);
    font-size: 0.9em;
  }
</style>
