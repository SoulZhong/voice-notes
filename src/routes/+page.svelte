<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { listNotes } from "$lib/notes";
  import { hasNavigated } from "$lib/navIntent";
  import { t } from "$lib/i18n/index.svelte";

  let empty = $state(false);

  onMount(async () => {
    try {
      const notes = await listNotes();
      // 落地重定向只在"没人明确要去别处"时才做(Codex P2):listNotes 是异步的,等它的这段
      // 时间里用户/托盘可能已经把页面导走了(冷启动点托盘「打开设置」正是如此)——组件虽已
      // 销毁,这个 promise 仍会跑完,不设防就会把人从设置页又踢回 /record。
      // 两道都要:hasNavigated 挡住"已 goto 但 history 还没更新"的那一拍(goto 是异步的),
      // pathname 挡住不经 navIntent 的其它导航(比如用户自己点侧栏)。
      if (hasNavigated() || window.location.pathname !== "/") return;
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
