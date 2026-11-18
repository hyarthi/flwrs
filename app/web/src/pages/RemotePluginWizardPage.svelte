<script lang="ts">
  import Page from '../components/Page.svelte';
  import Placeholder from '../components/Placeholder.svelte';
</script>

<Page>
  <Placeholder
    title="New remote plugin"
    items={[
      'Describes a plugin that runs elsewhere, for remote nodes',
      'Steps: identity, payload schemas, node settings, UI, review',
    ]}
  />
</Page>
