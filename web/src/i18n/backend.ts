import { ref } from 'vue';
import { language, type Lang } from './index';

// Messages written by the backend (Rust: drivers, commands, dbine-schema…)
// reach the UI as Spanish sentences. They're translated here instead of in
// Rust, so no driver has to change (drivers are versioned and published on
// their own).
//
// `scripts/i18n/extract_backend.py` collects them from the Rust sources into
// `locales/backend.msgids.json`; each language has `locales/<lang>/backend.json`
// mapping a message to its translation. A message made with `format!` is a
// pattern: its values are `{0}`, `{1}`… (`no existe la base {0}`), and each
// value is translated again (it's often another backend message, e.g. the
// reason after "no se pudo conectar: "). What isn't in the catalog stays as
// it came (server errors, which the engine writes in its own language).

type Catalog = Record<string, string>;
const loaders = import.meta.glob<{ default: Catalog }>('../locales/*/backend.json');

interface Compiled {
  exact: Map<string, string>;
  patterns: { re: RegExp; to: string }[];
}
const loaded = new Map<Lang, Compiled>();
/** Bumped when a catalog arrives, so what was rendered before it re-renders. */
const version = ref(0);

function compile(cat: Catalog): Compiled {
  const exact = new Map<string, string>();
  const patterns: { re: RegExp; to: string; literal: number }[] = [];
  for (const [from, to] of Object.entries(cat)) {
    if (!to) continue;
    if (!/\{\d+\}/.test(from)) {
      exact.set(from, to);
      continue;
    }
    const parts = from.split(/(\{\d+\})/);
    const src = parts
      .map((p) => (/^\{\d+\}$/.test(p) ? `(?<v${p.slice(1, -1)}>[\\s\\S]*?)` : p.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')))
      .join('');
    try {
      patterns.push({ re: new RegExp(`^${src}$`), to, literal: from.replace(/\{\d+\}/g, '').length });
    } catch { /* a pattern the engine can't compile: skip it */ }
  }
  // The most specific first (more fixed text wins over a looser pattern).
  patterns.sort((a, b) => b.literal - a.literal);
  return { exact, patterns };
}

/** Load the current language's catalog (at start and on every switch). */
export async function loadBackendCatalog(lang: Lang = language.value) {
  if (lang === 'es' || loaded.has(lang)) return;
  const load = loaders[`../locales/${lang}/backend.json`];
  if (!load) return;
  loaded.set(lang, compile((await load()).default));
  version.value++;
}

function translate(msg: string, c: Compiled, depth: number): string {
  const hit = c.exact.get(msg);
  if (hit !== undefined) return hit;
  if (depth > 3) return msg;
  // A message of several lines: each on its own.
  if (msg.includes('\n')) return msg.split('\n').map((l) => translate(l, c, depth + 1)).join('\n');
  for (const p of c.patterns) {
    const m = p.re.exec(msg);
    if (!m?.groups) continue;
    return p.to.replace(/\{(\d+)\}/g, (_, n: string) => {
      const v = m.groups![`v${n}`] ?? '';
      return v ? translate(v, c, depth + 1) : v;
    });
  }
  return msg;
}

/** A backend message in the current language (as it came when unknown). */
export function tb(msg: string | null | undefined): string {
  if (!msg) return msg ?? '';
  void version.value;
  const c = loaded.get(language.value);
  return c ? translate(msg, c, 0) : msg;
}
