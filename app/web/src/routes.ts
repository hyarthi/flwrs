import type { Component } from 'svelte';
import type { Params } from './lib/router.svelte';
import ScenesPage from './pages/ScenesPage.svelte';
import ScenePage from './pages/ScenePage.svelte';
import OverviewPage from './pages/OverviewPage.svelte';
import PluginsPage from './pages/PluginsPage.svelte';
import PluginPage from './pages/PluginPage.svelte';
import RemotePluginWizardPage from './pages/RemotePluginWizardPage.svelte';
import ServerPage from './pages/ServerPage.svelte';
import LoginPage from './pages/LoginPage.svelte';

export interface RouteDef {
  pattern: string;
  // Pages take their path parameters as props.
  component: Component<any>;
  title: string;
  /** The sidebar entry it belongs to. */
  section?: 'scenes' | 'overview' | 'plugins' | 'server';
  /** Rendered without the app frame (sidebar, top bar). */
  bare?: boolean;
  /** Fixed props, merged with the path parameters. */
  props?: Params;
  /**
   * Routes of one group show the same page instance while their `id`
   * parameter stays (e.g. a scene's tabs and its node modal), so moving
   * between them doesn't reload the page.
   */
  group?: string;
}

// First match wins. Ids sit under `by-id/`, as in the API, so a fixed path
// (e.g. `new-remote`) can never clash with an id.
export const routes: RouteDef[] = [
  { pattern: '/scenes', component: ScenesPage, title: 'Scenes', section: 'scenes' },
  { pattern: '/scenes/by-id/:id', component: ScenePage, title: 'Scene', section: 'scenes', props: { tab: 'layout' }, group: 'scene' },
  { pattern: '/scenes/by-id/:id/stats', component: ScenePage, title: 'Scene', section: 'scenes', props: { tab: 'stats' }, group: 'scene' },
  { pattern: '/scenes/by-id/:id/nodes/by-id/:nodeId', component: ScenePage, title: 'Scene', section: 'scenes', props: { tab: 'layout' }, group: 'scene' },
  { pattern: '/overview', component: OverviewPage, title: 'Overview', section: 'overview' },
  { pattern: '/plugins', component: PluginsPage, title: 'Plugins', section: 'plugins' },
  { pattern: '/plugins/new-remote', component: RemotePluginWizardPage, title: 'New remote plugin', section: 'plugins' },
  { pattern: '/plugins/by-id/:id', component: PluginPage, title: 'Plugin', section: 'plugins' },
  { pattern: '/plugins/by-id/:id/edit', component: RemotePluginWizardPage, title: 'Edit remote plugin', section: 'plugins' },
  { pattern: '/server', component: ServerPage, title: 'Server', section: 'server' },
  { pattern: '/login', component: LoginPage, title: 'Log in', bare: true },
];

/** Where `/` goes: the scenes list for now (maybe a dashboard later). */
export const HOME = '/scenes';
