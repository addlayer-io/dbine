// The "Enviar un mensaje" step's checks in the editor: the same rules the
// backend applies when the task is saved and run
// (src-tauri/src/tasks/message.rs), said before the user saves. The
// backend stays the authority (it also checks the addresses a name resolves to).

export type MessageChannel = 'teams' | 'slack' | 'telegram' | 'ntfy';
export const MESSAGE_CHANNELS: MessageChannel[] = ['teams', 'slack', 'telegram', 'ntfy'];

export type MessageProblem =
  | 'missingUrl'
  | 'badUrl'
  | 'notHttps'
  | 'teamsHost'
  | 'slackHost'
  | 'ntfyLocal'
  | 'ntfyTopic'
  | 'missingToken'
  | 'badToken'
  | 'missingChat'
  | 'badChat';

/** Microsoft's domains for a Teams "Workflows" webhook. */
export const TEAMS_DOMAINS = ['logic.azure.com', 'api.powerplatform.com', 'webhook.office.com'];

const under = (host: string, domain: string) => host === domain || host.endsWith(`.${domain}`);

/** This computer, the local network or a cloud's metadata service. */
export function localHost(host: string): boolean {
  const h = host.replace(/^\[|\]$/g, '').replace(/\.$/, '').toLowerCase();
  if (h === 'localhost' || h === 'metadata' || ['.localhost', '.local', '.internal', '.home.arpa'].some((s) => h.endsWith(s))) return true;
  const v4 = h.match(/^(\d+)\.(\d+)\.(\d+)\.(\d+)$/);
  if (v4) {
    const [a, b] = [Number(v4[1]), Number(v4[2])];
    return a === 0 || a === 10 || a === 127 || a >= 224 || (a === 169 && b === 254) || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168) || (a === 100 && b >= 64 && b <= 127);
  }
  if (h.includes(':')) return h === '::' || h === '::1' || /^f[cd]/.test(h) || /^fe[89ab]/.test(h) || h.startsWith('::ffff:');
  return false;
}

function urlProblem(channel: MessageChannel, url: string): MessageProblem | null {
  let u: URL;
  try { u = new URL(url.trim()); } catch { return 'badUrl'; }
  if (u.protocol !== 'https:') return 'notHttps';
  if (u.username || u.password) return 'badUrl';
  const host = u.hostname.toLowerCase();
  if (channel === 'teams' && (!TEAMS_DOMAINS.some((d) => under(host, d)) || u.port)) return 'teamsHost';
  if (channel === 'slack' && (host !== 'hooks.slack.com' || u.port)) return 'slackHost';
  if (channel === 'ntfy') {
    if (localHost(host)) return 'ntfyLocal';
    const topic = u.pathname.split('/').pop() ?? '';
    if (!/^[A-Za-z0-9_-]{1,64}$/.test(topic) || u.search) return 'ntfyTopic';
  }
  return null;
}

/**
 * What's wrong with the step's settings. `keptChannel`: the channel the
 * saved secrets were typed for (null: nothing saved); an empty secret field
 * on that channel means "unchanged".
 */
export function messageProblems(config: Record<string, any>, keptChannel: string | null): MessageProblem[] {
  const channel = (config.channel ?? 'teams') as MessageChannel;
  const kept = keptChannel === channel;
  const url = String(config.url ?? '').trim();
  const token = String(config.token ?? '').trim();
  const chat = String(config.chat_id ?? '').trim();
  const out: MessageProblem[] = [];
  if (channel === 'telegram') {
    if (!token) { if (!kept) out.push('missingToken'); }
    else if (!/^\d+:[A-Za-z0-9_-]+$/.test(token)) out.push('badToken');
    if (!chat) out.push('missingChat');
    else if (!/^-?\d{1,20}$/.test(chat) && !/^@[A-Za-z0-9_]{4,64}$/.test(chat)) out.push('badChat');
    return out;
  }
  if (!url) { if (!kept) out.push('missingUrl'); }
  else {
    const p = urlProblem(channel, url);
    if (p) out.push(p);
  }
  if (channel === 'ntfy' && token && /[\s\x00-\x1f]/.test(token)) out.push('badToken');
  return out;
}
