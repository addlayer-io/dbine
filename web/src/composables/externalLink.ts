import { ElMessage } from 'element-plus';
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

/** A click inside rendered Markdown: a link's click opens it outside. */
export function onMarkdownClick(e: MouseEvent) {
  const a = (e.target as HTMLElement | null)?.closest?.('a[href]') as HTMLAnchorElement | null;
  if (!a) return;
  e.preventDefault();
  e.stopPropagation();
  void openExternalLink(a.getAttribute('href') ?? '');
}
