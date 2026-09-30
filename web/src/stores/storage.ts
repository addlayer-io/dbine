// localStorage for per-viewer conveniences (layout, open tabs). It may be
// unavailable, so every access is guarded and callers get the default.

export function readJson<T>(key: string, def: T): T {
  try {
    const v = localStorage.getItem(key);
    return v === null ? def : (JSON.parse(v) as T);
  } catch { return def; }
}

export function writeJson(key: string, v: unknown) {
  try { localStorage.setItem(key, JSON.stringify(v)); } catch { /* ignore */ }
}
