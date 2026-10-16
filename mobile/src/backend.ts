import i18next from 'i18next';

// DBine's texts (Monitor labels, notes, the channel's errors) arrive in
// Spanish; this translates them with the slice of DBine's catalog the build
// copies (scripts/catalog.py). Same rules as the desktop's `tb`: an exact
// match, else a message with `{n}` parts, each part translated in turn.

const catalogs = import.meta.glob<Record<string, string>>('./locales/backend/*.json', { eager: true, import: 'default' });

interface Compiled {
  exact: Map<string, string>;
  patterns: { re: RegExp; to: string }[];
}

const compiled = new Map<string, Compiled>();

function compile(lang: string): Compiled | null {
  const raw = catalogs[`./locales/backend/${lang}.json`];
  if (!raw) return null;
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
  const lang = i18next.language;
  if (lang === 'es') return msg;
  if (!compiled.has(lang)) {
    const c = compile(lang);
    if (!c) return msg;
    compiled.set(lang, c);
  }
  return translate(msg, compiled.get(lang)!, 0);
}
