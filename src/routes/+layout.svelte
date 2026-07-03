<script lang="ts">
  import { onMount } from "svelte";
  import Sidebar from "$lib/Sidebar.svelte";
  import { recording } from "$lib/recording.svelte";

  let { children } = $props();

  onMount(() => {
    recording.init();
  });
</script>

<div class="shell">
  <Sidebar />
  <main class="main">
    {@render children()}
  </main>
</div>

<style>
  :global(body) {
    margin: 0;
    background: #ffffff;
    color: #1a1a1a;
  }
  @media (prefers-color-scheme: dark) {
    :global(body) {
      background: #1c1c1e;
      color: #f0f0f0;
    }
  }
  .shell {
    display: flex;
    height: 100vh;
    font-family: -apple-system, system-ui, sans-serif;
  }
  .main {
    flex: 1;
    overflow-y: auto;
    min-width: 0;
  }
</style>
