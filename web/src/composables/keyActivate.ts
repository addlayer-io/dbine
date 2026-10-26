import type { Directive } from 'vue';

// `v-key-activate`: a clickable element that isn't a <button> (a list row,
// a card, a sortable table header) takes the keyboard focus, and Enter or
// Space clicks it, so it works without a mouse.
//
//   v-key-activate               role="button"
//   v-key-activate="'checkbox'"  that role instead
//   v-key-activate.native        keeps the element's own role (<th>, <tr>)
//   v-key-activate.open          Enter also double-clicks (rows that open on
//                                double click); Space only clicks
//
// Keys pressed inside a field or button within the element are left alone.

function onKey(e: KeyboardEvent) {
  const el = e.currentTarget as HTMLElement;
  if (e.target !== el || e.metaKey || e.ctrlKey || e.altKey) return;
  if (e.key !== 'Enter' && e.key !== ' ') return;
  e.preventDefault();
  el.click();
  if (e.key === 'Enter' && el.dataset.keyOpen !== undefined) {
    el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true, cancelable: true, detail: 2 }));
  }
}

export const vKeyActivate: Directive<HTMLElement, string | undefined> = {
  mounted(el, binding) {
    if (!binding.modifiers.native && !el.hasAttribute('role')) el.setAttribute('role', binding.value ?? 'button');
    if (!el.hasAttribute('tabindex')) el.tabIndex = 0;
    if (binding.modifiers.open) el.dataset.keyOpen = '';
    el.addEventListener('keydown', onKey);
  },
  beforeUnmount(el) {
    el.removeEventListener('keydown', onKey);
  },
};
