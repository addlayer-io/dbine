import { defineStore } from 'pinia';
import { ElNotification } from 'element-plus';
import { listen } from '@tauri-apps/api/event';
import { locale, t } from '../i18n';
import { errorMessage } from '../api/client';
import {
  CONNECTION_OPTION, PROFILE_OPTION, enterpriseApi,
  type EnterpriseConnection, type EnterpriseDuration, type EnterpriseEvent, type EnterpriseProfile, type EnterpriseStatus, type RunningStatement,
} from '../api/enterprise';
import { useConnectionsStore } from './connections';
import { runningStatements } from './tabs';
import type { SavedConnection } from '../api/types';

// Signed in to DBine Enterprise: the company's catalog (the explorer's
// Enterprise tab), the access requests and extensions asked from here, and
// the notices DBine Enterprise sends, worded in the user's language and
// shown in the app and, when DBine isn't in front, by the system.

/** What the request / extension dialog is for. */
export interface AccessTarget {
  mode: 'request' | 'extend';
  connection: EnterpriseConnection;
  profile: EnterpriseProfile;
  /** It's asked to use the connection outside its allowed hours. */
  outsideHours?: boolean;
}

/** Off in published builds: only a build made with `VITE_DBINE_ENTERPRISE=1`
 *  shows any of this. */
export const ENTERPRISE_ENABLED = import.meta.env.VITE_DBINE_ENTERPRISE === '1';

export const useEnterpriseStore = defineStore('enterprise', {
  state: () => ({
    status: null as EnterpriseStatus | null,
    started: false,
    signInOpen: false,
    access: null as AccessTarget | null,
  }),
  getters: {
    signedIn: (s) => !!s.status?.signed_in,
    connections: (s): EnterpriseConnection[] => s.status?.catalog?.connections ?? [],
    organization: (s) => s.status?.organization || s.status?.catalog?.organization || 'DBine Enterprise',
    settings: (s) => s.status?.catalog?.settings ?? null,
  },
  actions: {
    /** Once per run: the current status and the backend's events. */
    async init() {
      if (this.started) return;
      this.started = true;
      try {
        this.status = await enterpriseApi.status();
      } catch {
        /* not ready: the first status event fills it */
      }
      await listen<EnterpriseStatus>('enterprise-status', (e) => {
        const before = JSON.stringify(this.status?.catalog?.connections ?? null);
        this.status = e.payload;
        // The managed connections changed with the catalog: the explorer lists them again.
        if (JSON.stringify(e.payload.catalog?.connections ?? null) !== before) void useConnectionsStore().load();
      });
      await listen<EnterpriseEvent>('enterprise-event', (e) => this.tell(e.payload));
    },

    async signIn(server: string) {
      this.status = await enterpriseApi.signIn(server, t('enterprise:signIn.donePage'));
      await useConnectionsStore().load();
    },

    async signOut() {
      await enterpriseApi.signOut();
      this.status = await enterpriseApi.status();
      await useConnectionsStore().load();
    },

    async sync() {
      this.status = await enterpriseApi.sync();
    },

    /** The company connection and profile behind a managed connection. */
    of(conn: SavedConnection | undefined): { connection: EnterpriseConnection; profile: EnterpriseProfile } | null {
      const pid = conn?.config.options?.[PROFILE_OPTION];
      const cid = conn?.config.options?.[CONNECTION_OPTION];
      const connection = this.connections.find((c) => c.id === cid);
      const profile = connection?.profiles.find((p) => p.id === pid);
      return connection && profile ? { connection, profile } : null;
    },

    findProfile(profileId: string): { connection: EnterpriseConnection; profile: EnterpriseProfile } | null {
      for (const connection of this.connections) {
        const profile = connection.profiles.find((p) => p.id === profileId);
        if (profile) return { connection, profile };
      }
      return null;
    },

    openRequest(connection: EnterpriseConnection, profile: EnterpriseProfile, outsideHours = false) {
      this.access = { mode: 'request', connection, profile, outsideHours };
    },

    openExtension(profileId: string) {
      const found = this.findProfile(profileId);
      if (found) this.access = { mode: 'extend', ...found };
    },

    /** What runs now on a profile's connection, for an extension. */
    running(profileId: string): RunningStatement[] {
      const id = `ent-${profileId}`;
      const now = Date.now();
      return [...runningStatements.values()]
        .filter((r) => r.connectionId === id)
        .map((r) => ({ statement: r.sql, seconds: Math.round((now - r.since) / 1000) }));
    },

    async request(profileId: string, databases: string[] | null, d: EnterpriseDuration, reason: string, outsideHours = false) {
      await enterpriseApi.request(profileId, databases, d, reason, outsideHours);
      this.status = await enterpriseApi.status();
    },

    async extend(profileId: string, d: EnterpriseDuration, reason: string) {
      await enterpriseApi.extend(profileId, d, reason, this.running(profileId));
      this.status = await enterpriseApi.status();
    },

    async cancelRequest(requestId: string) {
      await enterpriseApi.cancelRequest(requestId);
      this.status = await enterpriseApi.status();
    },

    /** A notice from DBine Enterprise, in the user's language. */
    tell(e: EnterpriseEvent) {
      // The event's data, as the server sends it (the request and grant
      // of a decision; the connection and profile of the rest).
      interface Data {
        connection?: string; profile?: string; profile_id?: string; minutes?: number; finish_minutes?: number; reason?: string;
        grant?: { expires_at?: string | null };
        request?: { connection?: string; profile?: string; profile_id?: string; extension?: boolean; decided_by?: string | null };
      }
      const d = (e.data ?? {}) as Data;
      const req = d.request ?? {};
      const connection = String(d.connection ?? req.connection ?? '');
      const profile = String(d.profile ?? req.profile ?? '');
      const profileId = String(d.profile_id ?? req.profile_id ?? '');
      const when = (iso: unknown) => (iso ? new Date(String(iso)).toLocaleString(locale(), { dateStyle: 'medium', timeStyle: 'short' }) : '');
      let type: 'success' | 'warning' | 'info' | 'error' = 'info';
      let body = '';
      let onClick: (() => void) | undefined;
      switch (e.kind) {
        case 'request.approved':
          type = 'success';
          body = req.extension
            ? t('enterprise:notice.extended', { connection, profile, until: when(d.grant?.expires_at) || t('enterprise:noExpiry') })
            : d.grant?.expires_at
              ? t('enterprise:notice.approved', { connection, profile, until: when(d.grant.expires_at) })
              : t('enterprise:notice.approvedForever', { connection, profile });
          break;
        case 'request.rejected':
          type = 'warning';
          body = t(req.extension ? 'enterprise:notice.extensionRejected' : 'enterprise:notice.rejected', { connection, profile, who: req.decided_by ?? '' });
          break;
        case 'request.expired':
          body = t('enterprise:notice.requestExpired', { connection, profile });
          break;
        case 'grant.added':
          type = 'success';
          body = d.grant?.expires_at
            ? t('enterprise:notice.granted', { connection, profile, until: when(d.grant.expires_at) })
            : t('enterprise:notice.grantedForever', { connection, profile });
          break;
        case 'grant.revoked':
          type = 'warning';
          body = t('enterprise:notice.revoked', { connection, profile });
          break;
        case 'grant.ending': {
          type = 'warning';
          const running = this.running(profileId);
          const minutes = Number(d.minutes ?? 0);
          if (running.length) {
            const longest = Math.max(...running.map((r) => r.seconds));
            body = t('enterprise:notice.endingRunning', { connection, profile, count: minutes, running: elapsed(longest) });
          } else {
            body = t('enterprise:notice.ending', { connection, profile, count: minutes });
          }
          body += ` ${t('enterprise:notice.clickToExtend')}`;
          onClick = () => this.openExtension(profileId);
          break;
        }
        case 'schedule.ending': {
          type = 'warning';
          body = `${t('enterprise:notice.scheduleEnding', { connection, profile, count: Number(d.minutes ?? 0) })} ${t('enterprise:notice.clickToRequestHours')}`;
          const found = this.findProfile(profileId);
          if (found) onClick = () => this.openRequest(found.connection, found.profile, true);
          break;
        }
        case 'session.draining':
          type = 'warning';
          body = `${t('enterprise:notice.draining', { connection, profile, count: Number(d.finish_minutes ?? 0) })} ${t('enterprise:notice.clickToExtend')}`;
          onClick = () => this.openExtension(profileId);
          break;
        case 'session.ended':
          type = 'warning';
          body = t(d.reason === 'revoked' ? 'enterprise:notice.endedRevoked' : d.reason === 'outside_hours' ? 'enterprise:notice.endedOutsideHours' : 'enterprise:notice.ended', { connection, profile });
          break;
        case 'session.lost':
          type = 'warning';
          body = t('enterprise:notice.sessionLost');
          onClick = () => { this.signInOpen = true; };
          break;
        default:
          return;
      }
      const title = this.organization;
      ElNotification({ type, title, message: body, duration: type === 'success' ? 8000 : 0, onClick });
      if (!document.hasFocus()) void enterpriseApi.notify(title, body).catch(() => {});
    },
  },
});

/** "1 h 40 min", "25 min", "40 s". */
function elapsed(seconds: number): string {
  if (seconds < 60) return t('enterprise:elapsed.seconds', { count: seconds });
  const m = Math.floor(seconds / 60);
  if (m < 60) return t('enterprise:elapsed.minutes', { count: m });
  return t('enterprise:elapsed.hours', { h: Math.floor(m / 60), m: m % 60 });
}

export { errorMessage };
