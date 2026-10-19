import i18next from 'i18next';

// DBine's texts (Monitor labels, notes, the channel's errors) arrive in
// Spanish; this translates them with the slice of DBine's catalog the build
// copies (scripts/catalog.py). Same rules as the desktop's `tb`: an exact
// match, else a message with `{n}` parts, each part translated in turn.

// One chunk per language: only the phone's language is loaded (loadCatalog).
const catalogs = import.meta.glob<Record<string, string>>('./locales/backend/*.json', { import: 'default' });

interface Compiled {
  exact: Map<string, string>;
  patterns: { re: RegExp; to: string }[];
}

const compiled = new Map<string, Compiled>();

function compile(raw: Record<string, string>): Compiled {
  const exact = new Map<string, string>();
  const patterns: { re: RegExp; to: string }[] = [];
  for (const [from, to] of Object.entries(raw)) {
    if (!/\{\d+\}/.test(from)) {
      exact.set(from, to);
      continue;
    }
    const src = from.split(/(\{\d+\})/).map((part) => {
      const m = /^\{(\d+)\}$/.exec(part);
      return m ? `(?<v${m[1]}>.+?)` : part.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    });
    try {
      patterns.push({ re: new RegExp(`^${src.join('')}$`, 's'), to });
    } catch {
      /* a repeated group name: skip */
    }
  }
  return { exact, patterns };
}

/** Load the phone language's catalog. Called once before the app mounts
 *  (the language doesn't change while the app runs). */
export async function loadCatalog(lang: string = i18next.language) {
  if (lang === 'es' || compiled.has(lang)) return;
  const load = catalogs[`./locales/backend/${lang}.json`];
  if (load) compiled.set(lang, compile(await load()));
}

function translate(msg: string, c: Compiled, depth: number): string {
  const hit = c.exact.get(msg);
  if (hit !== undefined) return hit;
  if (depth > 3) return msg;
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

/** A DBine text in the phone's language (as it came when unknown). */
export function tb(msg: string | null | undefined): string {
  if (!msg) return msg ?? '';
  const c = compiled.get(i18next.language);
  return c ? translate(msg, c, 0) : msg;
}
