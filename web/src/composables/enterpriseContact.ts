// "DBine Enterprise para equipos" (EnterpriseSignInDialog): a company that
// wants it writes to the public support address; the e-mails that arrive are
// how interest is measured (no telemetry). Plain functions, no Vue.

/** The address dbine.com publishes for support and contact. */
export const ENTERPRISE_CONTACT = 'support@dbine.com';

/** The e-mail to write, with its subject already filled in. */
export function contactUrl(subject: string): string {
  return `mailto:${ENTERPRISE_CONTACT}?subject=${encodeURIComponent(subject)}`;
}

/** The window opens on the sign-in form, not the pitch, for whoever already
 *  used a company server on this computer (signed out since). */
export function opensOnSignIn(savedServer: string | null | undefined): boolean {
  return !!savedServer?.trim();
}
