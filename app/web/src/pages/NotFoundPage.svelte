<script lang="ts">
  import Page from '../components/Page.svelte';
</script>

<Page>
  <div class="card empty">
    <h2>Page not found</h2>
    <p class="muted">Nothing lives at this address.</p>
    <a class="btn" href="/">Go to the scenes</a>
  </div>
</Page>

<style>
  .empty {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--space-3);
    max-width: 480px;
    padding: var(--space-5);
  }
</style>
