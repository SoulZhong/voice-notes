<script lang="ts">
  import "../app.css";
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
    background: var(--canvas);
    color: var(--ink);
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
