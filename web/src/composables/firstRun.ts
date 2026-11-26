import { ref } from 'vue';

// The first launch shows one thing at a time, importing first: the offer to
// import other tools' connections (ImportSuggestion) decides before the
// telemetry notice (TelemetryConsent) may show.

/** The import offer is still checking or open. */
export const importOfferPending = ref(true);

/** The import offer is done: answered, not needed, or not possible now. */
export function importOfferDone() {
  importOfferPending.value = false;
}
