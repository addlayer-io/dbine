// The sample databases DBine adds once (src-tauri/src/samples.rs,
// docs/sample-databases.md): their fixed ids and their texts in the UI's
// language (the backend's `Texts`, snake_case). Pure, so the tests run it
// without the app.

/** The folder and its connections: fixed ids, the same on every computer. */
export const SAMPLES_FOLDER_ID = 'dbine-samples';
export const SAMPLE_CONNECTION_IDS = ['dbine-sample-tienda', 'dbine-sample-analitica', 'dbine-sample-archivos'] as const;
/** The preference that says they were added (whatever happened since). */
export const SAMPLES_INSTALLED = 'samples.installed';

export interface SampleTexts {
  folder: string;
  tienda: string;
  analitica: string;
  archivos: string;
  /** The tour's file name, without the extension. */
  tour_file: string;
  tour: { intro: string; differences: string; only_in_analitica: string; targets: string };
}

type T = (key: string, values?: Record<string, unknown>) => string;

/** What the backend writes the folder, the connections and the tour with. */
export function sampleTexts(t: T): SampleTexts {
  const names = { tienda: t('samples:tienda'), analitica: t('samples:analitica'), archivos: t('samples:archivos') };
  return {
    folder: t('samples:folder'),
    ...names,
    tour_file: t('samples:tourFile'),
    tour: {
      intro: t('samples:tour.intro', names),
      differences: t('samples:tour.differences', names),
      only_in_analitica: t('samples:tour.onlyInAnalitica', names),
      targets: t('samples:tour.targets', names),
    },
  };
}

/** One of the sample connections (telemetry counts them apart). */
export const isSampleConnection = (id: string) => (SAMPLE_CONNECTION_IDS as readonly string[]).includes(id);
