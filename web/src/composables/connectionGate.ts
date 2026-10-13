import { ref } from 'vue';
import { ElMessage } from 'element-plus';
import { t } from '../i18n';
import { useConnectionsStore } from '../stores/connections';

/** A view's way to the connection: `ensure()` connects (or reuses the live
 *  connection) and `blocked` says the view is waiting for it or couldn't get
 *  it — the view then shows <ConnectionState> (Conectando…, or the error with
 *  "Ver qué pasa…" and "Reintentar") instead of an empty pane. Never a
 *  silent return. */
export function useConnectionGate(connectionId: () => string) {
  const conns = useConnectionsStore();
  const blocked = ref(false);
  /** `auto`: a repeated, unattended call (a poll): after a failure it
   *  doesn't connect again until "Reintentar". */
  async function ensure(opts: { auto?: boolean } = {}): Promise<boolean> {
    const id = connectionId();
    if (conns.isConnected(id)) {
      blocked.value = false;
      return true;
    }
    blocked.value = true;
    const ok = await conns.ensureConnected(id, opts);
    blocked.value = !ok;
    return ok;
  }
  return { blocked, ensure };
}

/** For a connection a view or dialog only picks (a target, the other side of
 *  a compare): connect, and when it can't, say so with the server's message
 *  instead of doing nothing. */
export async function ensureConnectedOrTell(connectionId: string): Promise<boolean> {
  const conns = useConnectionsStore();
  if (await conns.ensureConnected(connectionId)) return true;
  const name = conns.byId(connectionId)?.name ?? connectionId;
  ElMessage.error({ message: t('connection:problem.couldNotConnect', { name, error: conns.live[connectionId]?.error ?? t('common:error') }), duration: 6000, showClose: true });
  return false;
}
