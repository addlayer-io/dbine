import { invoke } from '@tauri-apps/api/core';

// Power BI's admin API (the scanner behind the lineage search): the consent of `Tenant.Read.All`
// (read-only), asked on request, never with the sign-in.
export const powerbiAdminApi = {
  /** "Autorizar la API de administración…": opens the browser once on a Power BI connection. Resolves
   *  when the account has granted it; rejects with Entra's refusal in plain words (not an administrator). */
  authorize: (connectionId: string) =>
    invoke<void>('powerbi_authorize_admin', { args: { connection_id: connectionId } }),
};
