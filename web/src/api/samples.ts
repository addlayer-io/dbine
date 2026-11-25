import { invoke } from '@tauri-apps/api/core';
import type { SampleTexts } from '../composables/sampleDatabases';

// The sample databases (src-tauri/src/samples.rs).
export const samplesApi = {
  /** Adds them once; the tour's path when it did now, else null. */
  install: (texts: SampleTexts) => invoke<string | null>('samples_install', { args: { texts } }),
  /** The tour's path, written again if it was deleted. */
  tour: (texts: SampleTexts) => invoke<string>('samples_tour', { args: { texts } }),
  /** Deletes their connections, folder and files; they don't come back. */
  remove: () => invoke<void>('samples_remove'),
};
