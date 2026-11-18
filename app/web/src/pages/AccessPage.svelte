<script lang="ts">
  // Shown instead of the app when the server doesn't let this client in (no
  // secret key configured and not on loopback) or can't be reached.
  import logo from '../assets/logo.svg';
  import { session } from '../lib/session.svelte';

  const refused = $derived(session.status === 'refused');
  let busy = $state(false);

  async function retry() {
    busy = true;
    try {
      await session.load();
    } finally {
      busy = false;
    }
  }
</script>

<div class="access">
  <div class="card panel">
    <img src={logo} alt="" width="48" height="48" />
    <h1>{refused ? 'No access from here' : 'Can’t reach the server'}</h1>
    {#if session.problem}
      <p>{session.problem.message}</p>
      {#each session.problem.details as detail, i (i)}
        <p class="muted">{detail}</p>
      {/each}
    {/if}
    <button class="btn" type="button" onclick={retry} disabled={busy}>Try again</button>
  </div>
</div>

<style>
  .access {
    display: grid;
    place-items: center;
    height: 100%;
    padding: var(--space-5);
  }

  .panel {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--space-3);
    width: min(440px, 100%);
    padding: var(--space-6) var(--space-5);
    text-align: center;
  }

  img {
    border-radius: 10px;
  }

  p {
    font-size: var(--text-sm);
  }
</style>
