// `?lang=en|es|pt|fr|it|de|ru|zh|ja|ko` picks the preview's language (imported before ../i18n).
const lang = new URLSearchParams(location.search).get('lang');
if (lang) try { localStorage.setItem('dbine.language', lang); } catch { /* storage blocked */ }
