import { ElMessage } from 'element-plus';
import { invoke } from '@tauri-apps/api/core';
import { save } from '@tauri-apps/plugin-dialog';
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
