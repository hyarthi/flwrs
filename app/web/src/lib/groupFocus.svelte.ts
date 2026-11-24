// Which group the pointer is on (a chip or a legend entry). Shared through
// context, so hovering re-renders only what reads it, not the whole graph.

import { getContext, setContext } from 'svelte';

export class GroupFocus {
  id = $state<string | null>(null);
}

const KEY = Symbol('group-focus');

export function provideGroupFocus(): GroupFocus {
  return setContext(KEY, new GroupFocus());
}

export const useGroupFocus = () => getContext<GroupFocus | undefined>(KEY);
