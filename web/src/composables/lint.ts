import type { Diagnostic } from '@codemirror/lint';
import { lintApi, type LintFinding, type LintRule } from '../api/lint';
import { t } from '../i18n';
import { useSettingsStore } from '../stores/settings';

// "Calidad de código" (docs: the rule list per engine is in the report of
// src-tauri/src/lint). The analysis runs in Rust; this turns its findings
// into editor diagnostics and applies the switches from Settings.

/** State settings: the linter on/off, and the rules turned off. */
export const LINT_ENABLED = 'lint.enabled';
export const LINT_DISABLED = 'lint.disabled';

/** Rules checked in the editor itself, against the explorer's objects
 *  (composables/nameRefs.ts): listed in Configuración with the backend's. */
export const LOCAL_LINT_RULES: LintRule[] = [
  { id: 'unknown-table', severity: 'warning', groups: ['sql', 'cql'] },
  { id: 'unknown-column', severity: 'warning', groups: ['sql', 'cql'] },
];

/** A finding with its message, for the "Ver problemas" list. */
export interface LintProblem extends LintFinding {
  message: string;
}

export function lintMessage(f: LintFinding): string {
  return t(`lint:rules.${f.rule}.message`, { ...f.params, interpolation: { escapeValue: false } });
}

export function lintWhy(rule: string): string {
  return t(`lint:rules.${rule}.why`);
}

/** The hover text: the message, why it matters and how to fix it. */
function render(f: LintFinding, message: string): () => Node {
  return () => {
    const box = document.createElement('div');
    box.className = 'cm-lint-dbine';
    const head = document.createElement('div');
    head.textContent = message;
    head.style.fontWeight = '600';
    const why = document.createElement('div');
    why.textContent = lintWhy(f.rule);
    why.style.marginTop = '4px';
    why.style.maxWidth = '460px';
    why.style.whiteSpace = 'normal';
    const id = document.createElement('div');
    id.textContent = t('lint:ruleId', { id: f.rule });
    id.style.marginTop = '4px';
    id.style.opacity = '0.6';
    id.style.fontSize = '11px';
    box.append(head, why, id);
    return box;
  };
}

/** The editor's lint source for a connection, or null when it's off.
 *  `onProblems` gets every run's result (for "Ver problemas"); `local` adds
 *  the editor's own findings (unknown names). */
export function lintSourceFor(
  connectionId: string,
  onProblems: (p: LintProblem[]) => void,
  local?: ((doc: string) => LintFinding[]) | null,
): ((doc: string) => Promise<Diagnostic[]>) | null {
  const settings = useSettingsStore();
  if (!connectionId || !settings.get<boolean>(LINT_ENABLED, true)) {
    onProblems([]);
    return null;
  }
  const off = new Set(settings.get<string[]>(LINT_DISABLED, []));
  return async (doc: string) => {
    let found: LintFinding[];
    try {
      found = await lintApi.lint(connectionId, doc);
    } catch {
      // A connection whose driver isn't installed, a deleted one…: no marks.
      found = [];
    }
    if (local) found = [...found, ...local(doc)].sort((a, b) => a.start - b.start);
    const problems = found.filter((f) => !off.has(f.rule)).map((f) => ({ ...f, message: lintMessage(f) }));
    onProblems(problems);
    return problems.map((f) => {
      const from = Math.min(f.start, doc.length);
      return {
        from,
        to: Math.min(Math.max(f.end, from), doc.length),
        severity: f.severity,
        source: f.rule,
        message: f.message,
        renderMessage: render(f, f.message),
      };
    });
  };
}
