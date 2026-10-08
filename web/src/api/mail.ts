import { invoke } from '@tauri-apps/api/core';

// Configuración › Correo: the SMTP server the "Enviar un mail" steps of the
// scheduled tasks use. Mirrors src-tauri/src/tasks/mail.rs and
// src-tauri/src/commands/mail.rs.

export type MailSecurity = 'starttls' | 'tls' | 'none';

export interface MailSettings {
  host: string;
  port: number;
  security: MailSecurity;
  /** Empty: the server takes mail without logging in. */
  user: string;
  from_address: string;
  from_name: string;
}

export interface MailView {
  /** null: not configured yet. */
  settings: MailSettings | null;
  /** The password is in the vault ("guardada"). */
  password_saved: boolean;
}

export const mailApi = {
  get: () => invoke<MailView>('mail_settings_get'),
  /** An empty password keeps the saved one. */
  save: (settings: MailSettings, password: string) =>
    invoke<MailView>('mail_settings_save', { args: { settings, password: password || null } }),
  /** Sends a test mail with the form as it is; the answer is the summary. */
  test: (settings: MailSettings, password: string, to: string) =>
    invoke<string>('mail_test', { args: { settings, password: password || null, to } }),
};

export function newMailSettings(): MailSettings {
  return { host: '', port: 587, security: 'starttls', user: '', from_address: '', from_name: 'DBine' };
}

/** The usual port of each security. */
export const MAIL_PORTS: Record<MailSecurity, number> = { starttls: 587, tls: 465, none: 25 };
