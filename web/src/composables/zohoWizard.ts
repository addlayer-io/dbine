// "Conectar Zoho Vault" (components/ZohoConnectWizard.vue): the steps and
// what each needs before going on. Pure, for the wizard and its tests.
// The labels quoted are Zoho's own (API console, checked against
// zoho.com/accounts/protocol/oauth/self-client/*).

/** Where the Self Client is created. */
export const ZOHO_API_CONSOLE = 'https://api-console.zoho.com/';
/** The scopes the code is generated with (src-tauri: zoho::SCOPES). */
export const ZOHO_SCOPES = 'ZohoVault.secrets.READ,ZohoVault.user.READ';
/** The description suggested for the code. */
export const ZOHO_CODE_DESCRIPTION = 'DBine';

export type WizardStep = 1 | 2 | 3 | 4;
export type WizardMaster = 'keychain' | 'ask' | 'command';

export interface WizardState {
  step: WizardStep;
  client_id: string;
  client_secret: string;
  code: string;
  /** Step 3 done: the exchange's pending ID (nothing saved yet). */
  pending: string | null;
  data_center: string;
  identity: string;
  label: string;
  master: WizardMaster;
  master_command: string;
  passphrase: string;
}

export function emptyWizard(): WizardState {
  return {
    step: 1, client_id: '', client_secret: '', code: '', pending: null, data_center: '', identity: '', label: '',
    master: 'keychain', master_command: '', passphrase: '',
  };
}

/** A client ID as the console shows it (`1000.XXXX`). */
export function validClientId(id: string): boolean {
  return /^[A-Za-z0-9._-]{1,200}$/.test(id.trim());
}

/** What step `step` still lacks (input names; `client_id_format` for a malformed ID). */
export function stepProblems(s: WizardState, step: WizardStep = s.step): string[] {
  const out: string[] = [];
  const empty = (v: string) => v.trim() === '';
  switch (step) {
    case 1:
      if (empty(s.client_id)) out.push('client_id');
      else if (!validClientId(s.client_id)) out.push('client_id_format');
      if (empty(s.client_secret)) out.push('client_secret');
      break;
    case 2:
      if (empty(s.code)) out.push('code');
      else if (/\s/.test(s.code.trim())) out.push('code_format');
      break;
    case 3:
      if (!s.pending) out.push('connect');
      if (empty(s.label)) out.push('label');
      break;
    case 4:
      if (s.master === 'command' ? empty(s.master_command) : s.passphrase === '') out.push(s.master === 'command' ? 'master_command' : 'passphrase');
      break;
  }
  return out;
}

export function canAdvance(s: WizardState): boolean {
  return stepProblems(s).length === 0;
}

/** Going back to steps 1–2 drops the exchange (the code is spent; a new one is needed). */
export function back(s: WizardState): { dropPending: string | null } {
  if (s.step === 1) return { dropPending: null };
  s.step = (s.step - 1) as WizardStep;
  if (s.step <= 2 && s.pending) {
    const p = s.pending;
    s.pending = null;
    s.data_center = '';
    s.code = '';
    return { dropPending: p };
  }
  return { dropPending: null };
}

/** Steps 1, 2 and 4 go on when complete; step 3 when the exchange is done. */
export function next(s: WizardState): boolean {
  if (!canAdvance(s) || s.step === 4) return false;
  s.step = (s.step + 1) as WizardStep;
  return true;
}

/** The label offered after the exchange: the user's e-mail, or what was there. */
export function suggestedLabel(identity: string, current: string): string {
  if (current.trim()) return current;
  return identity ? `Zoho Vault · ${identity}` : '';
}

/** Backend messages that send the user back to a step. */
export function stepForError(message: string): WizardStep | null {
  if (message.includes('generá un código nuevo') || message.includes('volvé a generar el código')) return 2;
  if (message.includes('«Client ID»') || message.includes('«Client Secret»')) return 1;
  return null;
}
