<script lang="ts">
  import Page from '../components/Page.svelte';
  import Placeholder from '../components/Placeholder.svelte';

  let { id }: { id: string } = $props();
</script>

<Page>
  <Placeholder
    title={`Plugin ${id}`}
    items={['Manifest, docs, payload schemas, node settings', 'The nodes using it', 'Upgrade (a new package for the same id)']}
  />
</Page>
