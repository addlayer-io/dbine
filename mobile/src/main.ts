import { createApp } from 'vue';
import I18NextVue from 'i18next-vue';
import i18next from './i18n';
import App from './App.vue';
import './style.css';

createApp(App).use(I18NextVue, { i18next }).mount('#app');
