import i18next from 'i18next';
import es from './locales/es.json';
import en from './locales/en.json';
import pt from './locales/pt.json';
import fr from './locales/fr.json';
import it from './locales/it.json';

// The phone's language, as DBine's five (Spanish is the source).
const LANGS = ['es', 'en', 'pt', 'fr', 'it'];
const phone = (navigator.language || 'es').slice(0, 2).toLowerCase();

i18next.init({
  lng: LANGS.includes(phone) ? phone : 'en',
  fallbackLng: 'es',
  resources: { es: { translation: es }, en: { translation: en }, pt: { translation: pt }, fr: { translation: fr }, it: { translation: it } },
  interpolation: { escapeValue: false },
});

export default i18next;
