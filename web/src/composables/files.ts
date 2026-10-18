import { ElMessage } from 'element-plus';
import { invoke } from '@tauri-apps/api/core';
import { open, save } from '@tauri-apps/plugin-dialog';
import { errorMessage } from '../api/client';
import { t } from '../i18n';

/** Ask where to save and write `text` there (native save dialog). */
export async function saveTextFile(text: string, defaultName: string, filters: { name: string; extensions: string[] }[]) {
  try {
    const path = await save({ defaultPath: defaultName, filters });
    if (!path) return;
    await invoke('save_text_file', { args: { path, contents: text } });
    ElMessage.success({ message: t('core:files.saved'), duration: 1500 });
  } catch (e) {
    ElMessage.error(t('core:files.saveFailed', { error: errorMessage(e) }));
  }
}

/** Same, for binary content given as base64 (images). */
export async function saveBinaryFile(base64: string, defaultName: string, filters: { name: string; extensions: string[] }[]) {
  try {
    const path = await save({ defaultPath: defaultName, filters });
    if (!path) return;
    await invoke('save_binary_file', { args: { path, base64 } });
    ElMessage.success({ message: t('core:files.saved'), duration: 1500 });
  } catch (e) {
    ElMessage.error(t('core:files.saveFailed', { error: errorMessage(e) }));
  }
}

/** A data file for a notebook's file readers (native open dialog): its path,
 *  or null. Choosing it grants nothing: the notebook asks for each file it
 *  reads when the cell runs. */
export async function pickDataFile(): Promise<string | null> {
  try {
    const picked = await open({
      multiple: false,
      directory: false,
      filters: [
        { name: t('editor:dataFiles'), extensions: ['csv', 'tsv', 'txt', 'parquet', 'json', 'jsonl', 'ndjson', 'xlsx', 'gz', 'zst'] },
        { name: t('scripts:run.allFiles'), extensions: ['*'] },
      ],
    });
    return typeof picked === 'string' ? picked : null;
  } catch (e) {
    ElMessage.error(errorMessage(e));
    return null;
  }
}
