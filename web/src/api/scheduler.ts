import { invoke } from '@tauri-apps/api/core';
import type { MonitorTable } from './types';

// A server's scheduler managed whole: every kind of object it has, listed,
// created, edited and deleted through a form the driver describes.
// Copied by hand from crates/dbine-driver/src/scheduler.rs.

/** One kind of object the server's scheduler has. Labels in Spanish: translate with `tb`. */
export interface SchedulerKind {
  /** "job", "schedule", "operator", "alert", "proxy", "category", "program"… */
  key: string;
  label: string;
  singular: string;
  can_create?: boolean;
  can_edit?: boolean;
  can_delete?: boolean;
  /** What else can be done with its objects (run now, open a window…): shown as a script, confirmed. */
  actions?: SchedulerAction[];
  /** Its objects (jobs) start at one of their steps (`schedulerApi.runFrom`). */
  run_from_step?: boolean;
  /** Its objects keep a run history the user can delete (`schedulerApi.clearHistory`). */
  clear_history?: boolean;
}

/** An action on an object besides editing and deleting it. Label in Spanish: translate with `tb`. */
export interface SchedulerAction {
  /** What `schedulerApi.action` takes: "run", "open_window", "start"… */
  key: string;
  label: string;
}

/** What the server's scheduler has. */
export interface SchedulerInfo {
  /** In the order the tree shows them (jobs first). */
  kinds: SchedulerKind[];
  /** It keeps a log of its own (`schedulerApi.logs`): SQL Server Agent's error log. */
  logs?: boolean;
  /** In Spanish from the driver (the scheduler stopped, an edition without one…): translate with `tb`. */
  notes?: string[];
}

export interface SchedulerItem {
  /** The engine's id for it, opaque: what the other calls take. */
  id: string;
  name: string;
  description: string | null;
  /** null: the kind has no enabled state. */
  enabled: boolean | null;
  system?: boolean;
  /** Absent: as its kind says; false: not this one (a system category, another plugin's monitor…). */
  can_edit?: boolean | null;
  can_delete?: boolean | null;
  /** The keys of its kind's actions it offers now; absent: all of them. */
  actions?: string[] | null;
}

export interface SchedulerList {
  items: SchedulerItem[];
  /** In Spanish from the driver: translate with `tb`. */
  notes?: string[];
}

/** How a field is edited. Option labels in Spanish: translate with `tb`. */
export type FormKind =
  | { type: 'text' }
  | { type: 'textarea' }
  | { type: 'number'; min?: number | null; max?: number | null }
  | { type: 'bool' }
  | { type: 'select'; options: [string, string][] }
  | { type: 'multi_select'; options: [string, string][] }
  /** Picks from `ObjectForm.choices[source]`; a source `list:<key>` picks a row of the same
   *  form's list `<key>` as edited now, by its `title_field` value (a job's start step). A source
   *  may name fields between braces (`perf_counter:{object}`): filled from the same row, else the
   *  form's values, before looking it up; with nothing listed for it, the field takes free text. */
  | { type: 'ref'; source: string; multiple?: boolean }
  | { type: 'code'; language: string }
  /** YYYY-MM-DD */
  | { type: 'date' }
  /** HH:MM:SS */
  | { type: 'time' }
  /** RFC 3339 */
  | { type: 'date_time' }
  /** Seconds. */
  | { type: 'duration' }
  | { type: 'cron' }
  /** A password or token: the form never carries its value (null); send a value only when the
   *  user typed one (null keeps the server's). Scripts show it masked. */
  | { type: 'secret' };

/** Labels, help and placeholders in Spanish from the driver: translate with `tb`. */
export interface FormField {
  key: string;
  label: string;
  kind: FormKind;
  required?: boolean;
  help?: string | null;
  placeholder?: string | null;
  read_only?: boolean;
  /** Shown only while that field (of the same form or row) has one of these values. */
  when?: [string, string[]] | null;
  /** The value of a new object or row. */
  default?: unknown;
  /** More conditions like `when`, all of which must hold too. */
  when_all?: [string, string[]][];
}

export type FormRow = Record<string, unknown>;

export interface FormList {
  key: string;
  label: string;
  fields: FormField[];
  rows?: FormRow[];
  /** The order of the rows means something (steps run in it). */
  orderable?: boolean;
  min_rows?: number;
  /** The field that names a row in the list's table. */
  title_field: string;
  /** Rows can't be added or removed. */
  read_only?: boolean;
  /** Shown (and sent) only while that top-level field has one of these values. */
  when?: [string, string[]] | null;
}

export interface FormSection {
  key: string;
  label: string;
  fields?: FormField[];
  lists?: FormList[];
  /** The tab shows only while that top-level field has one of these values. */
  when?: [string, string[]] | null;
}

export interface ObjectForm {
  sections: FormSection[];
  values?: Record<string, unknown>;
  /** `ref` source → [value, label] pairs. */
  choices?: Record<string, [string, string][]>;
  /** In Spanish from the driver: translate with `tb`. */
  notes?: string[];
  /** Nothing in it can be saved (permissions, a system object, read-only connection). */
  read_only?: boolean;
}

/** What the designer sends back: top-level values and each list's rows, in their order. */
export interface FormValues {
  values: Record<string, unknown>;
  lists: Record<string, FormRow[]>;
}

const a = (connectionId: string, extra: Record<string, unknown> = {}) => ({ args: { connection_id: connectionId, ...extra } });

export const schedulerApi = {
  info: (connectionId: string) => invoke<SchedulerInfo>('scheduler_info', a(connectionId)),
  items: (connectionId: string, kind: string) => invoke<SchedulerList>('scheduler_items', a(connectionId, { kind })),
  /** `id` null: the form of a new object. */
  form: (connectionId: string, kind: string, id: string | null) =>
    invoke<ObjectForm>('scheduler_form', a(connectionId, { kind, id })),
  /** The script for the designer's values; nothing runs. */
  script: (connectionId: string, kind: string, id: string | null, values: FormValues) =>
    invoke<string>('scheduler_script', a(connectionId, { kind, id, values })),
  /** Creates or changes the object; returns the new object's id. */
  save: (connectionId: string, kind: string, id: string | null, values: FormValues) =>
    invoke<string | null>('scheduler_save', a(connectionId, { kind, id, values })),
  deleteScript: (connectionId: string, kind: string, id: string) =>
    invoke<string>('scheduler_delete_script', a(connectionId, { kind, id })),
  delete: (connectionId: string, kind: string, id: string) => invoke<void>('scheduler_delete', a(connectionId, { kind, id })),
  /** The script of one of the kind's actions on an object; nothing runs. */
  actionScript: (connectionId: string, kind: string, id: string, action: string) =>
    invoke<string>('scheduler_action_script', a(connectionId, { kind, id, action })),
  /** Runs one of the kind's actions on an object (after the user confirmed its script). */
  action: (connectionId: string, kind: string, id: string, action: string) =>
    invoke<void>('scheduler_action', a(connectionId, { kind, id, action })),
  /** Starts the job at one of its steps. */
  runFrom: (connectionId: string, id: string, step: string) => invoke<void>('jobs_run_from', a(connectionId, { id, step })),
  clearHistory: (connectionId: string, id: string) => invoke<void>('jobs_clear_history', a(connectionId, { id })),
  /** The scheduler's own logs (SQL Server Agent's error logs…). */
  logs: (connectionId: string) => invoke<MonitorTable[]>('scheduler_logs', a(connectionId)),
};
