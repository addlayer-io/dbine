// The API-key form's model list (AiApiKeyDialog): what "Probar" found joins
// what the saved provider already listed, without repeats, the typed one
// first. Plain functions, no Vue.

/** The models to offer: the typed one, the ones the test found, then the known ones. */
export function offeredModels(typed: string, tested: string[], known: string[]): string[] {
  const out: string[] = [];
  for (const m of [typed.trim(), ...tested, ...known]) if (m && !out.includes(m)) out.push(m);
  return out;
}

/** After removing a provider's key: the chat's chosen provider, cleared when
 *  it was that one (the chat then uses the first one available). */
export function providerAfterRemove(chosen: string | null, removed: string): string | null {
  return chosen === removed ? null : chosen;
}
