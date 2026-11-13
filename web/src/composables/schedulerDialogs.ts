import { computed, onBeforeUnmount, onMounted, reactive } from 'vue';
import type { FormValues, ObjectForm } from '../api/scheduler';
import { t } from '../i18n';
import { confirmNative } from '../native';
import { tb } from '../i18n/backend';
import { scriptWarnings } from './schedulerForm';

// The scheduler's dialogs, asked from anywhere (the sidebar's menus, a job's
// tab, the designer): a script to look at, or to run after the user's yes,
// "Iniciar en el paso…"'s step list, and the form of an action that asks
// for values first. The questions live here; one
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

/** The form of an action that asks for values first (a time range, job
 *  parameters, a chain's step), drawn by SchedulerActionForm.vue. */
export interface FormRequest {
  /** The action's label. */
  title: string;
  /** What it's for, naming the object and the connection. */
  message?: string;
  /** As the driver describes it for that object. */
  form: ObjectForm;
  /** The OK button: the action's label. */
  okLabel: string;
  /** The SQL dialect for code fields (the driver's). */
  dialect: string;
  /** Runs on OK with the form's values (checked) while the dialog waits:
   *  the script, its confirmation and the run. True: done, the dialog
   *  closes; false: the user said no to the script, the form stays. What it
   *  throws shows in the dialog, which keeps the values for a retry. */
  submit: (values: FormValues) => Promise<boolean>;
}

export const schedulerDialogs = reactive({
  script: null as (ScriptRequest & { resolve: (ok: boolean) => void }) | null,
  step: null as (StepRequest & { resolve: (step: string | null) => void }) | null,
  form: null as (FormRequest & { resolve: (done: boolean) => void }) | null,
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
    const warned = scriptWarnings(r.script).map((w) => tb(w));
    return confirmNative([r.message, r.warning, r.notice, ...warned].filter(Boolean).join('\n\n'), {
      title: r.title,
      okLabel: warned.length ? t('scheduled:designer.continue') : (r.okLabel ?? t('common:accept')),
      kind: r.danger || warned.length ? 'warning' : 'info',
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

/** Shows an action's form; resolves true once its `submit` finished (the
 *  action ran), false when the user closed it. */
export function askForm(r: FormRequest): Promise<boolean> {
  // Nothing to draw it (shouldn't happen): nothing runs.
  if (!schedulerDialogs.hosts.length) return Promise.resolve(false);
  schedulerDialogs.form?.resolve(false);
  return new Promise((resolve) => { schedulerDialogs.form = { ...r, resolve }; });
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

export function answerForm(done: boolean) {
  const r = schedulerDialogs.form;
  schedulerDialogs.form = null;
  r?.resolve(done);
}
