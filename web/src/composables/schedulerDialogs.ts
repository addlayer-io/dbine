import { computed, onBeforeUnmount, onMounted, reactive } from 'vue';
import { t } from '../i18n';
import { confirmNative } from '../native';

// The scheduler's dialogs, asked from anywhere (the sidebar's menus, a job's
// tab, the designer): a script to look at, or to run after the user's yes,
// and "Iniciar en el paso…"'s step list. The questions live here; one
// <SchedulerDialogs> draws them. Several views mount one (the sidebar and
// the tabs come and go on their own), and only the first one mounted shows
// them, so a question never opens twice.

export interface ScriptRequest {
  /** 'view': the script with Copiar and Abrir en una consulta; 'confirm':
   *  the change it makes, run only on OK. */
  mode: 'view' | 'confirm';
  title: string;
  /** What runs, naming the object and the connection. */
  message?: string;
  /** The production notice, when the connection is one. */
  warning?: string | null;
  /** Something to know about the script (a typed secret shows masked). */
  notice?: string | null;
  script: string;
  /** The SQL dialect to color it with (the driver's). */
  dialect: string;
  okLabel?: string;
  danger?: boolean;
  /** "Abrir en una consulta" (view mode). */
  openInQuery?: () => void;
}

export interface StepRequest {
  title: string;
  message: string;
  steps: { name: string; kind: string | null }[];
  okLabel: string;
}

export const schedulerDialogs = reactive({
  script: null as (ScriptRequest & { resolve: (ok: boolean) => void }) | null,
  step: null as (StepRequest & { resolve: (step: string | null) => void }) | null,
  /** The mounted <SchedulerDialogs>, oldest first: the first one draws. */
  hosts: [] as number[],
});

let seq = 0;

/** For <SchedulerDialogs>: whether this one is the one that draws. */
export function useDialogsHost() {
  const id = ++seq;
  onMounted(() => schedulerDialogs.hosts.push(id));
  onBeforeUnmount(() => {
    const i = schedulerDialogs.hosts.indexOf(id);
    if (i >= 0) schedulerDialogs.hosts.splice(i, 1);
  });
  return computed(() => schedulerDialogs.hosts[0] === id);
}

/** Shows a script; in 'confirm' mode resolves true when the user said yes. */
export function askScript(r: ScriptRequest): Promise<boolean> {
  if (!schedulerDialogs.hosts.length) {
    // Nothing to draw it (shouldn't happen): at least the question.
    if (r.mode === 'view') return Promise.resolve(false);
    return confirmNative([r.message, r.warning, r.notice].filter(Boolean).join('\n\n'), {
      title: r.title, okLabel: r.okLabel ?? t('common:accept'), kind: r.danger ? 'warning' : 'info',
    });
  }
  schedulerDialogs.script?.resolve(false);
  return new Promise((resolve) => { schedulerDialogs.script = { ...r, resolve }; });
}

/** The step to start a job at, or null. */
export function askStep(r: StepRequest): Promise<string | null> {
  schedulerDialogs.step?.resolve(null);
  return new Promise((resolve) => { schedulerDialogs.step = { ...r, resolve }; });
}

export function answerScript(ok: boolean) {
  const r = schedulerDialogs.script;
  schedulerDialogs.script = null;
  r?.resolve(ok);
}

export function answerStep(step: string | null) {
  const r = schedulerDialogs.step;
  schedulerDialogs.step = null;
  r?.resolve(step);
}
