import { invoke } from '@tauri-apps/api/core';
import type { ObjectRef } from './types';

// Users and permissions (docs/users-and-permissions.md).

export interface Principal {
  name: string;
  kind: 'user' | 'role';
  can_login: boolean | null;
  superuser: boolean | null;
  disabled: boolean | null;
  member_of: string[];
  details: [string, string][];
  system: boolean;
}

export interface Grant {
  privilege: string;
  object: string | null;
  object_kind: string | null;
  grantable: boolean;
  denied: boolean;
  via: string | null;
}

export type SecurityAction =
  | { action: 'create_user'; name: string; password: string | null }
  | { action: 'create_role'; name: string }
  | { action: 'drop'; name: string; kind: 'user' | 'role' }
  | { action: 'set_password'; name: string; password: string }
  | { action: 'set_login'; name: string; enabled: boolean }
  | { action: 'grant'; privileges: string[]; object: ObjectRef | null; to: string; grantable: boolean }
  | { action: 'revoke'; privileges: string[]; object: ObjectRef | null; from: string }
  | { action: 'add_member'; role: string; member: string }
  | { action: 'remove_member'; role: string; member: string };

export const securityApi = {
  principals: (connectionId: string, database: string) =>
    invoke<Principal[]>('security_principals', { args: { connection_id: connectionId, database } }),
  grants: (connectionId: string, database: string, principal: string) =>
    invoke<Grant[]>('security_grants', { args: { connection_id: connectionId, database, principal } }),
  /** `shown` hides the password (for the preview). */
  script: (connectionId: string, action: SecurityAction) =>
    invoke<{ script: string; shown: string }>('security_script', { args: { connection_id: connectionId, action } }),
};
