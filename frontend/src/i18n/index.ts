import i18n from 'i18next'
import { initReactI18next } from 'react-i18next'
import en from './en.json'

// Minimal setup: a single bundled locale, no HTTP backend or browser
// language detection yet (Fable V-C3 -- structure now, translate later).
// Add i18next-http-backend + i18next-browser-languagedetector when a
// second locale is actually needed; not before, since neither has been
// verified to work correctly inside Tauri's asset-serving webview.
void i18n.use(initReactI18next).init({
  resources: {
    en: { translation: en },
  },
  lng: 'en',
  fallbackLng: 'en',
  interpolation: {
    escapeValue: false, // React already escapes.
  },
})

export default i18n
