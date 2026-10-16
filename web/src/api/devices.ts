import { invoke } from '@tauri-apps/api/core';

// Configuración › Dispositivos: the phone app's server and the paired
// phones (src-tauri/src/link.rs, docs/mobile-companion.md).

export interface Device {
  id: string;
  name: string;
  key: string;
  paired_at: string;
  last_seen: string | null;
  can_kill: boolean;
}

export interface LinkStatus {
  enabled: boolean;
  port: number;
  running: boolean;
  error: string | null;
  /** This computer's key, to compare with the phone's screen. */
  fingerprint: string;
  devices: Device[];
}

export interface LinkInvite {
  svg: string;
  fingerprint: string;
  seconds: number;
}

/** A phone read the code and asks to pair: the user answers. */
export const LINK_PAIR_EVENT = 'link-pair-request';
export const LINK_CHANGED_EVENT = 'link-changed';
export interface PairAsk {
  name: string;
  fingerprint: string;
}

export const devicesApi = {
  status: () => invoke<LinkStatus>('link_status'),
  configure: (enabled: boolean, port: number) => invoke<LinkStatus>('link_configure', { args: { enabled, port } }),
  invite: () => invoke<LinkInvite>('link_invite'),
  cancelInvite: () => invoke<void>('link_cancel_invite'),
  answer: (accept: boolean) => invoke<void>('link_answer', { args: { accept } }),
  remove: (id: string) => invoke<LinkStatus>('link_remove_device', { args: { id } }),
  setCanKill: (id: string, canKill: boolean) => invoke<LinkStatus>('link_set_device', { args: { id, can_kill: canKill } }),
};
