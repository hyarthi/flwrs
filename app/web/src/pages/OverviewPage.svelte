<script lang="ts">
  import Page from '../components/Page.svelte';
  import Placeholder from '../components/Placeholder.svelte';
</script>

<Page>
  <Placeholder
    title="Overview"
    items={[
      'Scenes by state, problems, hub connections',
      'Queue budget, spill memory and disk, event rates',
      'Time graphs from live samples while the page is open (no server history)',
    ]}
  />
</Page>
