<script lang="ts">
  import Monitor from '@lucide/svelte/icons/monitor';
  import Sun from '@lucide/svelte/icons/sun';
  import Moon from '@lucide/svelte/icons/moon';
  import { theme } from '../lib/theme.svelte';

  const label = $derived(
    theme.current === 'system' ? 'Theme: system' : theme.current === 'light' ? 'Theme: light' : 'Theme: dark',
  );
</script>

<button class="btn ghost icon" type="button" title={label} aria-label={label} onclick={() => theme.cycle()}>
  {#if theme.current === 'system'}
    <Monitor size={18} strokeWidth={1.75} />
  {:else if theme.current === 'light'}
    <Sun size={18} strokeWidth={1.75} />
  {:else}
    <Moon size={18} strokeWidth={1.75} />
  {/if}
</button>
