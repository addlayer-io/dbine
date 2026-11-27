import { watch, type Ref } from 'vue';
import type { SavedQuery } from '../api/types';

/** An open query tab keeps its own copy of the saved query, and autosave
 *  writes that whole copy back: a rename made elsewhere (the explorer's menu)
 *  has to reach it, or the next keystroke restores the old name. Only the name
 *  is taken (the text is the editor's), and only when the list's entry
 *  changes it: what the tab just loaded wins over a list that was already
 *  there. `listed`: the query's entry in the explorer's lists. */
export function followRename(open: Ref<SavedQuery | null>, listed: () => SavedQuery | undefined) {
  watch(() => listed()?.name, (name, before) => {
    if (before === undefined || name === undefined || !open.value || name === open.value.name) return;
    open.value = { ...open.value, name };
  });
}
