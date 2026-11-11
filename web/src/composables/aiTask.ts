import type { Asker } from '../api/aiTask';
import { language } from '../i18n';
import { useAiStore } from '../stores/ai';

/** The provider and model the assistant uses, for a one-shot task; null
 *  when no AI is set up (the caller says so and opens the panel). */
export async function asker(): Promise<Asker | null> {
  const ai = useAiStore();
  await ai.init();
  const p = ai.provider;
  if (!p) return null;
  return { provider: p.kind, model: ai.model, ui_language: language.value, run_id: crypto.randomUUID() };
}

/** The provider's name, for "sent to …" notes. */
export function providerLabel(): string {
  return useAiStore().provider?.label ?? '';
}
