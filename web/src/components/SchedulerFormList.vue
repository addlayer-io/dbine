<script setup lang="ts">
import { computed, ref } from 'vue';
import { useTranslation } from 'i18next-vue';
import type { FormField, FormRow, FormList, ObjectForm } from '../api/scheduler';
import {
  canRemoveRow, humanDuration, isVisible, moveRow, newRow, optionLabel, problemText, refOptions, refsToCheck,
  renameRefs, rowProblems, summaryFields, whenText, type FormCtx, type Problem, type Problems, type RowRef, type Scope,
} from '../composables/schedulerForm';
import { tb } from '../i18n/backend';
import SchedulerFormFields from './SchedulerFormFields.vue';

// One list of a scheduler form (a job's steps and schedules, an alert's
// operators…): its table, with each row's title and up to three short
// columns, and the row editor ("Agregar", the pencil, a double click), a
// copy of the row written back on "Aceptar". Used by the designer and by an
// action's form (SchedulerActionForm.vue). A row renamed takes along what
// referred to it by its title (a job's start step, a step's "go to step"),
// so a change comes back whole: the form's top-level values and its lists.

const props = defineProps<{
  list: FormList;
  form: ObjectForm;
  /** The form's top-level values and every list's rows, as edited now. */
  values: Scope;
  lists: Record<string, FormRow[]>;
  /** The form's top level as conditions see it (`topContext`). */
  top: FormCtx;
  readOnly?: boolean;
  isNew?: boolean;
  dialect?: string;
  /** The form's problems once its checks ran, else null. */
  problems?: Problems | null;
}>();
const emit = defineEmits<{ change: [next: { values: Scope; lists: Record<string, FormRow[]> }] }>();
const { t } = useTranslation();

/** Rows can be added, removed and reordered. */
const editable = computed(() => !props.readOnly && !props.list.read_only);
const titleField = computed(() => props.list.fields.find((f) => f.key === props.list.title_field));
const rows = computed(() => props.lists[props.list.key] ?? []);
const columns = computed(() => summaryFields(props.list));
const rowCtx = (row: FormRow): FormCtx => ({ fields: props.list.fields, values: row, parent: props.top });
const canRemove = computed(() => canRemoveRow(props.list, rows.value.length, !!props.readOnly));

/** A `ref` field's choices where it is (`ctx`: its row; a source with
 *  `{field}`s fills from it), or null: free text. `skip`: the row being
 *  edited, left out of its own `list:` fields (a step doesn't go to itself). */
function choicesFor(f: FormField, ctx: FormCtx, skip?: RowRef): [string, string][] | null {
  return f.kind.type === 'ref' ? refOptions(f.kind.source, props.form, props.lists, ctx, skip) : [];
}

/** A summary column's cell: the field of that key that shows in this row
 *  (several may share a key under exclusive `when`s), or nothing. */
function summaryCell(c: FormField, row: FormRow): string {
  const ctx = rowCtx(row);
  const f = props.list.fields.find((x) => x.key === c.key && isVisible(x, ctx));
  return f ? cellText(f, row[c.key], ctx) : '';
}
const rowHasProblem = (i: number) => !!props.problems?.rows[props.list.key]?.some((p) => p.row === i);

/** A cell of the table, as the user reads it. A select's or a ref's value
 *  is looked up first: "" may be an option ("Cuenta de servicio del
 *  Agente"). */
function cellText(f: FormField | undefined, v: unknown, ctx: FormCtx): string {
  const k = f?.kind;
  if (f && (k?.type === 'select' || k?.type === 'multi_select' || k?.type === 'ref') && v !== null && v !== undefined) {
    const choices = choicesFor(f, ctx) ?? [];
    return (Array.isArray(v) ? v : [v]).map((x) => {
      const label = optionLabel(f, x, choices);
      return label === null ? whenText(x) : tb(label);
    }).join(', ');
  }
  if (v === null || v === undefined || v === '') return '';
  if (!f || !k) return typeof v === 'object' ? JSON.stringify(v) : String(v);
  if (k.type === 'bool') return v === true || v === 'true' ? t('common:yes') : t('common:no');
  if (k.type === 'duration' && typeof v === 'number') return humanDuration(v);
  if (k.type === 'secret') return '••••••';
  return typeof v === 'object' ? JSON.stringify(v) : String(v);
}

function setRows(next: FormRow[]) {
  emit('change', { values: props.values, lists: { ...props.lists, [props.list.key]: next } });
}

function move(i: number, dir: -1 | 1) {
  setRows(moveRow(rows.value, i, dir));
}

function removeRow(i: number) {
  if (!canRemove.value) return;
  setRows(rows.value.filter((_, j) => j !== i));
}

function rowProblemText(p: { row: number; field: string; problem: Problem }): string {
  const f = props.list.fields.find((x) => x.key === p.field);
  return t('scheduled:designer.errors.row', { n: p.row + 1, field: f ? tb(f.label) : p.field, error: problemText(p.problem, t) });
}

// The row editor: a copy of the row, written back on "Aceptar".
const rowEdit = ref<{ index: number | null; row: FormRow; showErrors: boolean } | null>(null);
const rowReadOnly = computed(() => !editable.value);
/** The edited row, left out of its own list's choices. */
const rowSkip = computed<RowRef | undefined>(() => (rowEdit.value ? { list: props.list.key, index: rowEdit.value.index } : undefined));
const rowChoicesFor = (f: FormField, ctx: FormCtx) => choicesFor(f, ctx, rowSkip.value);
const rowEditProblems = computed(() => {
  const r = rowEdit.value;
  if (!r) return null;
  const refs = refsToCheck(props.form, props.lists, rowSkip.value);
  return Object.fromEntries(rowProblems(props.list, r.row, props.top, !!props.isNew, refs).map((p) => [p.field, p.problem]));
});

function editRow(i: number) {
  rowEdit.value = { index: i, row: JSON.parse(JSON.stringify(rows.value[i])), showErrors: !!props.problems };
}

function addRow() {
  rowEdit.value = { index: null, row: newRow(props.list), showErrors: false };
}

function acceptRow() {
  const r = rowEdit.value;
  if (!r) return;
  if (rowReadOnly.value) { rowEdit.value = null; return; }
  if (Object.keys(rowEditProblems.value ?? {}).length) { r.showErrors = true; return; }
  const key = props.list.key;
  const next = rows.value.slice();
  const was = r.index === null ? '' : whenText(next[r.index]?.[props.list.title_field]);
  if (r.index === null) next.push(r.row);
  else next[r.index] = r.row;
  let values = props.values;
  let lists = { ...props.lists, [key]: next };
  // A row renamed: what referred to it by its title (a job's start step, the
  // steps' "go to step") follows the new one.
  const now = whenText(r.row[props.list.title_field]);
  if (was && now && was !== now) {
    const out = renameRefs(props.form, values, lists, key, was, now);
    if (out.count) {
      values = out.values;
      lists = out.lists;
    }
  }
  emit('change', { values, lists });
  rowEdit.value = null;
}

const rowTitle = computed(() => {
  const r = rowEdit.value;
  if (!r) return '';
  const v = r.row[props.list.title_field];
  const what = r.index === null ? t('scheduled:designer.newRow') : v ? String(v) : t('scheduled:designer.untitled');
  return `${tb(props.list.label)} · ${what}`;
});
</script>

<template>
  <section class="sfl">
    <div class="sfl-head">
      <h3 class="nm-section-title">{{ tb(list.label) }}</h3>
      <span class="sfl-count">{{ rows.length }}</span>
      <div class="sfl-sp" />
      <el-button v-if="editable" size="small" @click="addRow">
        <el-icon><ei-plus /></el-icon><span>{{ $t('scheduled:designer.add') }}</span>
      </el-button>
    </div>
    <table class="sfl-table">
      <thead>
        <tr>
          <th v-if="list.orderable" class="num">#</th>
          <th>{{ titleField ? tb(titleField.label) : list.title_field }}</th>
          <th v-for="c in columns" :key="c.key">{{ tb(c.label) }}</th>
          <th class="acts" />
        </tr>
      </thead>
      <tbody>
        <tr v-for="(r, i) in rows" :key="i" :class="{ invalid: rowHasProblem(i) }" @dblclick="editRow(i)">
          <td v-if="list.orderable" class="num">{{ i + 1 }}</td>
          <td class="title">{{ cellText(titleField, r[list.title_field], rowCtx(r)) || $t('scheduled:designer.untitled') }}</td>
          <td v-for="c in columns" :key="c.key">{{ summaryCell(c, r) }}</td>
          <td class="acts">
            <button
              class="sfl-icon"
              :title="editable ? $t('common:edit') : $t('scheduled:designer.view')"
              :aria-label="editable ? $t('common:edit') : $t('scheduled:designer.view')"
              @click="editRow(i)"
            ><el-icon><ei-edit v-if="editable" /><ei-view v-else /></el-icon></button>
            <template v-if="list.orderable && editable">
              <button class="sfl-icon" :disabled="i === 0" :title="$t('scheduled:designer.moveUp')" :aria-label="$t('scheduled:designer.moveUp')" @click="move(i, -1)"><el-icon><ei-top /></el-icon></button>
              <button class="sfl-icon" :disabled="i === rows.length - 1" :title="$t('scheduled:designer.moveDown')" :aria-label="$t('scheduled:designer.moveDown')" @click="move(i, 1)"><el-icon><ei-bottom /></el-icon></button>
            </template>
            <button
              v-if="editable"
              class="sfl-icon danger"
              :disabled="!canRemove"
              :title="canRemove ? $t('common:remove') : $t('scheduled:designer.minRowsHint', { count: list.min_rows ?? 0 })"
              :aria-label="$t('common:remove')"
              @click="removeRow(i)"
            ><el-icon><ei-delete /></el-icon></button>
          </td>
        </tr>
        <tr v-if="!rows.length">
          <td :colspan="columns.length + (list.orderable ? 3 : 2)" class="nm-muted">{{ $t('scheduled:designer.noRows') }}</td>
        </tr>
      </tbody>
    </table>
    <div v-if="problems?.lists[list.key]" class="sfl-err">{{ problemText(problems.lists[list.key], t) }}</div>
    <div v-for="(p, j) in problems?.rows[list.key] ?? []" :key="j" class="sfl-err">{{ rowProblemText(p) }}</div>

    <el-dialog
      :model-value="!!rowEdit"
      :title="rowTitle"
      width="min(860px, 92vw)"
      append-to-body
      :close-on-click-modal="false"
      @close="rowEdit = null"
    >
      <div v-if="rowEdit" class="sfl-row-editor">
        <SchedulerFormFields
          :fields="list.fields"
          :values="rowEdit.row"
          :ctx="rowCtx(rowEdit.row)"
          :problems="rowEdit.showErrors ? rowEditProblems : null"
          :read-only="rowReadOnly"
          :is-new="isNew"
          :dialect="dialect"
          :choices-for="rowChoicesFor"
          @update="(k, v) => { if (rowEdit) rowEdit.row[k] = v; }"
        />
      </div>
      <template #footer>
        <template v-if="rowReadOnly">
          <el-button type="primary" @click="rowEdit = null">{{ $t('common:close') }}</el-button>
        </template>
        <template v-else>
          <el-button @click="rowEdit = null">{{ $t('common:cancel') }}</el-button>
          <el-button type="primary" @click="acceptRow">{{ $t('common:accept') }}</el-button>
        </template>
      </template>
    </el-dialog>
  </section>
</template>

<style scoped>
.sfl-head { display: flex; align-items: center; gap: 8px; margin-bottom: 6px; }
.sfl-head .nm-section-title { margin: 0; }
.sfl-head .el-button .el-icon + span { margin-left: 4px; }
.sfl-sp { flex: 1; }
.sfl-count { font-size: 10.5px; min-width: 18px; padding: 0 5px; border-radius: 8px; text-align: center; color: var(--nm-text-dim); background: var(--ide-hover); }
.sfl-table { width: 100%; border-collapse: collapse; font-size: 12.5px; }
.sfl-table th { text-align: left; font-weight: 600; padding: 5px 10px; color: var(--nm-text-dim); border-bottom: 1px solid var(--nm-border); white-space: nowrap; }
.sfl-table td { padding: 4px 10px; border-bottom: 1px solid var(--nm-border-soft); color: var(--nm-text); max-width: 320px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sfl-table tbody tr:hover td { background: var(--ide-hover); }
.sfl-table tr.invalid td { background: color-mix(in srgb, var(--nm-danger) 10%, transparent); }
.sfl-table tr.invalid td.title { box-shadow: inset 2px 0 0 var(--nm-danger); }
.sfl-table .num { width: 28px; text-align: right; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.sfl-table td.title { color: var(--nm-text-strong); }
.sfl-table .acts { width: 1%; white-space: nowrap; text-align: right; padding: 2px 6px; }
.sfl-icon { display: inline-flex; align-items: center; justify-content: center; width: 22px; height: 22px; border: 0; border-radius: 4px; background: none; color: var(--nm-text-dim); cursor: pointer; }
.sfl-icon:hover:not(:disabled) { background: var(--ide-hover); color: var(--nm-text-strong); }
.sfl-icon.danger:hover:not(:disabled) { color: var(--nm-danger); }
.sfl-icon:disabled { opacity: .35; cursor: default; }
.sfl-err { margin-top: 4px; font-size: 11.5px; color: var(--nm-danger); }
.sfl-row-editor { max-height: 66vh; overflow: auto; padding-right: 6px; }
</style>
