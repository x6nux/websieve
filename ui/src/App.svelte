<script>
  import { invoke, listen } from './lib/ipc.js';

  let status = $state('未连接');
  let traffic = $state(null);

  $effect(() => {
    // IPC 往返自证：能拿到配置就说明 capability 配对了
    invoke('config_get')
      .then((cfg) => (status = `已加载配置：mixed-port ${cfg['mixed-port']}`))
      .catch((e) => (status = `配置读取失败：${e}`));

    const un = [];
    listen('status', (e) => (status = e.payload)).then((f) => un.push(f));
    listen('traffic', (e) => (traffic = e.payload)).then((f) => un.push(f));
    return () => un.forEach((f) => f());
  });
</script>

<main>
  <p class="status">{status}</p>
  {#if traffic}
    <p class="mono">↑ {traffic.up_rate} B/s ↓ {traffic.down_rate} B/s</p>
  {/if}
  <p class="hint">五个视图见阶段 5。</p>
</main>

<style>
  main {
    padding: var(--space-4);
  }
  .status {
    color: var(--text-1);
    font-size: var(--fs-14);
  }
  .mono {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    color: var(--text-2);
    font-size: var(--fs-13);
  }
  .hint {
    color: var(--text-4);
    font-size: var(--fs-12);
  }
</style>
