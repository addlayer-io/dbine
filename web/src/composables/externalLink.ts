import { h } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { invoke } from '@tauri-apps/api/core';
import { t } from '../i18n';
import { safeHref } from './markdown';

// Links in rendered Markdown (notebook text cells, the assistant's answers)
// open in the system's browser, never inside the app's window. The backend
// opens only http(s) and mailto addresses (`notebook_open_link`); when it
// can't, the address is copied so nothing is lost.

export async function openExternalLink(url: string) {
  const href = safeHref(url);
  if (!href) return;
  try {
    await invoke('notebook_open_link', { args: { url: href } });
  } catch {
    try {
      await navigator.clipboard.writeText(href);
      ElMessage.info({ message: t('notebook:link.copied', { url: href }), duration: 3500 });
    } catch { /* nothing more to do */ }
  }
}

/** A link in text DBine didn't write (the assistant's answers, which can
 *  carry what a prompt injection put there): it opens only after the user
 *  saw the whole address and said yes. */
export async function confirmExternalLink(url: string) {
  const href = safeHref(url);
  if (!href) return;
  try {
    await ElMessageBox.confirm(
      h('div', [
        h('p', { style: 'margin:0 0 8px' }, t('ai:link.body')),
        h('code', { style: 'display:block;white-space:pre-wrap;word-break:break-all;user-select:all' }, href),
      ]),
      t('ai:link.title'),
      {
        confirmButtonText: t('ai:link.open'),
        cancelButtonText: t('common:cancel'),
        type: 'warning',
        // No button focused: Enter doesn't open it by accident.
        autofocus: false,
      },
    );
  } catch {
    return;
  }
  await openExternalLink(href);
}

/** A click inside rendered Markdown: a link's click opens it outside
 *  (with `open`: `confirmExternalLink` for the assistant's answers). */
export function onMarkdownClick(e: MouseEvent, open: (url: string) => Promise<void> = openExternalLink) {
  const a = (e.target as HTMLElement | null)?.closest?.('a[href]') as HTMLAnchorElement | null;
  if (!a) return;
  e.preventDefault();
  e.stopPropagation();
  void open(a.getAttribute('href') ?? '');
}
