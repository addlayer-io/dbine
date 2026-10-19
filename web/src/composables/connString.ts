import { ref } from 'vue';

// "Copiar cadena de conexión…" (docs/connection-strings.md): the connection
// the dialog (components/ConnStringDialog.vue, mounted once in the explorer)
// is open on, from the explorer's menu or the connection form.

export const connStringTarget = ref<string | null>(null);

export function openConnString(connectionId: string) {
  connStringTarget.value = connectionId;
}

/** How long a copied string with a secret stays on the clipboard. */
export const CLEAR_AFTER_MS = 60_000;

let pending: ReturnType<typeof setTimeout> | null = null;

/**
 * Copy `text`; with a secret, clear the clipboard after CLEAR_AFTER_MS if it
 * still holds exactly that text. True when it will be cleared (the
 * clipboard could be read back to check it; otherwise it's left alone).
 */
export async function copyText(text: string, secret: boolean): Promise<boolean> {
  await navigator.clipboard.writeText(text);
  if (pending) clearTimeout(pending);
  pending = null;
  if (!secret) return false;
  pending = setTimeout(async () => {
    pending = null;
    try {
      if ((await navigator.clipboard.readText()) === text) await navigator.clipboard.writeText('');
    } catch { /* can't read it back: left as it is */ }
  }, CLEAR_AFTER_MS);
  return true;
}
