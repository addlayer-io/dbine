import { ElMessage } from 'element-plus';
import { errorMessage } from '../api/client';
import { samplesApi } from '../api/samples';
import { t } from '../i18n';
import { useTabsStore } from '../stores/tabs';
import { sampleTexts } from './sampleDatabases';
import { trackFeature } from './telemetry';

/** The sample databases' tour, in a tab (written again if it was deleted).
 *  From the folder's menu and the welcome screen; it never opens by itself. */
export async function openSampleTour() {
  try {
    useTabsStore().openNotebook({ path: await samplesApi.tour(sampleTexts(t)) });
    trackFeature('sample_tour');
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
