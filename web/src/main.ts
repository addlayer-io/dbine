import { createApp } from 'vue';
import { createPinia } from 'pinia';
import ElementPlus from 'element-plus';
import * as Icons from '@element-plus/icons-vue';
import 'element-plus/dist/index.css';
import 'element-plus/theme-chalk/dark/css-vars.css';
import './styles/global.scss';
import App from './App.vue';
import I18NextVue from 'i18next-vue';
import i18next from './i18n';
import { loadBackendCatalog } from './i18n/backend';
import { installNativeBehavior, showWindow } from './native';

document.documentElement.classList.add('dark');

const app = createApp(App);
app.config.errorHandler = (err, _vm, info) => {
  console.error(err);
  import('@tauri-apps/api/core')
    .then(({ invoke }) => invoke('log_ui_error', { message: `vue ${info}: ${err instanceof Error ? `${err.name}: ${err.message}\n${err.stack ?? ''}` : String(err)}` }))
    .catch(() => {});
};
app.use(createPinia());
app.use(ElementPlus, { size: 'small' });
app.use(I18NextVue, { i18next });
loadBackendCatalog();
for (const [name, comp] of Object.entries(Icons)) {
  app.component(`Ei${name}`, comp as any);
}
installNativeBehavior();
app.mount('#app');
// Dev test harness (src-tauri/src/devtools.rs): a PNG of the page.
if (import.meta.env.DEV) {
  (window as any).__dbineSnap = async () => (await import('html-to-image')).toPng(document.body, {
    pixelRatio: 1,
    // CodeMirror's src-less <img> widget buffers can't be captured.
    filter: (n) => !(n instanceof HTMLImageElement && !n.getAttribute('src')),
  });
}
showWindow();
