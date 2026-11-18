<script lang="ts">
  import Plus from '@lucide/svelte/icons/plus';
  import Page from '../components/Page.svelte';
  import Placeholder from '../components/Placeholder.svelte';
</script>

<Page>
  {#snippet actions()}
    <a class="btn" href="/plugins/new-remote"><Plus size={16} />New remote plugin</a>
  {/snippet}
  <Placeholder
    title="Plugins"
    items={['Installed plugins and remote plugin definitions: icon, name, version, type, platforms, nodes using them', 'Upload, delete']}
  />
</Page>
