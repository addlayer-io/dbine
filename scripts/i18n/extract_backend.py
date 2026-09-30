#!/usr/bin/env python3
"""Collect the backend's user-facing messages for translation.

The Rust code (drivers, commands, dbine-schema…) writes its messages in
Spanish; the UI translates them with `tb()` (web/src/i18n/backend.ts). This
script finds those messages in the Rust sources and writes them, normalized,
to web/src/locales/backend.msgids.json:

- string literals that read as Spanish text (not SQL, not identifiers);
- `format!` placeholders (`{}`, `{name}`, `{0:?}`, `{:.1}`…) become `{0}`,
  `{1}`… in order, which is how `tb()` matches them;
- `{{` / `}}` become literal braces;
- test code (`#[cfg(test)]` modules, `tests/` folders) is skipped.

Run it after changing backend messages, then translate what's new in each
web/src/locales/<lang>/backend.json (`python3 scripts/i18n/check.py` lists
what's missing).
"""

import json
import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parents[2]
OUT = ROOT / "web/src/locales/backend.msgids.json"

SPANISH = re.compile(
    r"[áéíóúñ¿¡ÁÉÍÓÚÑ]|\b(el|la|los|las|de|del|que|no|se|un|una|para|con|por|en|es|al|lo|sin|hay|como|más|este|esta)\b",
    re.I,
)
CODE_START = re.compile(
    r"^\s*(select|insert|update|delete|create|alter|drop|with|exec|execute|show|call|match|merge|grant|revoke|set|use|"
    r"begin|declare|db\.|pragma|explain|describe|desc|truncate|return|if|for|let|const|function)\b",
    re.I,
)
PLACEHOLDER = re.compile(r"\{\{|\}\}|\{[^{}]*\}")


def strip_tests(src: str) -> str:
    """Drop `#[cfg(test)]` items (test modules usually close the file)."""
    i = src.find("#[cfg(test)]")
    return src if i < 0 else src[:i]


def literals(src: str):
    """Plain "…" string literals (raw strings r#"…"# are SQL/JSON: skipped)."""
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        if c == "/" and src.startswith("//", i):
            i = src.find("\n", i)
            if i < 0:
                return
            continue
        if c == "/" and src.startswith("/*", i):
            j = src.find("*/", i + 2)
            i = n if j < 0 else j + 2
            continue
        if c == "r" and re.match(r'r#*"', src[i : i + 4]) and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            hashes = len(re.match(r"r(#*)", src[i:]).group(1))
            end = src.find('"' + "#" * hashes, i + 2 + hashes)
            i = n if end < 0 else end + 1 + hashes
            continue
        if c == "'" and i + 2 < n and (src[i + 2] == "'" or (src[i + 1] == "\\" and src[i + 3 : i + 4] == "'")):
            i += 3 if src[i + 2] == "'" else 4
            continue
        if c == '"':
            j, buf = i + 1, []
            while j < n and src[j] != '"':
                if src[j] == "\\" and j + 1 < n:
                    nxt = src[j + 1]
                    if nxt == "\n":  # line continuation: skip the newline and leading spaces
                        j += 2
                        while j < n and src[j] in " \t":
                            j += 1
                        continue
                    buf.append({"n": "\n", "t": "\t", '"': '"', "\\": "\\", "'": "'"}.get(nxt, nxt))
                    j += 2
                    continue
                buf.append(src[j])
                j += 1
            yield "".join(buf)
            i = j + 1
            continue
        i += 1


def normalize(s: str) -> str:
    k = 0

    def repl(m):
        nonlocal k
        t = m.group(0)
        if t == "{{":
            return "{"
        if t == "}}":
            return "}"
        out = "{" + str(k) + "}"
        k += 1
        return out

    return PLACEHOLDER.sub(repl, s)


def user_facing(s: str) -> bool:
    text = PLACEHOLDER.sub("", s).strip()
    if len(text) < 3 or not re.search(r"[A-Za-zÁÉÍÓÚáéíóúñÑ]", text):
        return False
    if CODE_START.match(text) or text.count(";") > 1:
        return False
    # Code templates for the editor (a comment line followed by code): the
    # code must reach the editor as is; not translated here.
    if "\n" in s and re.search(r"(?im)^\s*(select|create|insert|update|delete|alter|drop|with|db\.|match|call|begin)\b", s):
        return False
    # Code sent to a server (Lua for Redis) and prompts for the AI model: never shown.
    if "redis.call(" in s or s.startswith(("Sos el asistente", "cargo:")) or "<instrucciones>" in s:
        return False
    # A one-word label ("Tablas", "Sesiones", "Índices"): capitalized, no
    # camelCase. Product names and English words also match; their
    # translation keeps them as they are.
    # A short lowercase phrase label ("Tiempo activo", "Memoria usada").
    if re.fullmatch(r"[A-ZÁÉÍÓÚÑ][a-záéíóúñü]+( [a-záéíóúñü0-9()%/.-]+){1,5}", text):
        return True
    if re.fullmatch(r"[A-ZÁÉÍÓÚÑ][a-záéíóúñü]{2,}", text):
        return True
    if re.fullmatch(r"[\w.:/-]+", text) and not re.search(r"[áéíóúñ]", text):
        return False  # identifiers, paths, keys
    if " " not in text and not re.search(r"[áéíóúñ¿¡]", text):
        return False
    return bool(SPANISH.search(text))


# Plural units the drivers send as lowercase single words (Profiler column
# tooltips: `ProfilerStarted::units`); the filter above drops such words.
UNITS = ["páginas", "bloques", "filas", "bytes", "documentos", "claves"]


def main():
    msgs: dict[str, set[str]] = {}
    for u in UNITS:
        msgs.setdefault(u, set()).add("crates/dbine-driver/src/profiler.rs")
    files = [p for p in (ROOT / "crates").rglob("*.rs") if "/tests/" not in str(p) and p.name != "build.rs"]
    files += list((ROOT / "src-tauri/src").rglob("*.rs"))
    for f in sorted(files):
        src = strip_tests(f.read_text(encoding="utf-8"))
        for lit in literals(src):
            if user_facing(lit):
                msgs.setdefault(normalize(lit), set()).add(str(f.relative_to(ROOT)))
    out = {m: sorted(v) for m, v in sorted(msgs.items())}
    OUT.write_text(json.dumps(out, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    print(f"{len(out)} messages from {len(files)} files -> {OUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
