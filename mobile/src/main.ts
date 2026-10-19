import { createApp } from 'vue';
import I18NextVue from 'i18next-vue';
import i18next from './i18n';
import { loadCatalog } from './backend';
import App from './App.vue';
import './style.css';

// DBine's texts in the phone's language first, so they show translated.
loadCatalog()
  .catch((e) => console.error('catalog:', e))
  .then(() => createApp(App).use(I18NextVue, { i18next }).mount('#app'));
