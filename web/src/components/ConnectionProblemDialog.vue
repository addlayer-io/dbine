<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import type { AzureFirewallPlan, ConnectFailureKind } from '../api/types';
import { useConnectionsStore } from '../stores/connections';
import { useUiStore } from '../stores/ui';
import { isProdConnection } from '../composables/tags';

// "Ver qué pasa…": why a connection couldn't connect, in plain words, and
// what to do about it — by the kind of failure the backend classified
// (firewall, timeout, refused, dns, tls, auth). On Azure SQL's firewall it
// can add this machine's IP as a server firewall rule with the user's Azure
// CLI login, after a confirmation that shows the server, rule and IP (and,
// on a production connection, asks to type the server's name); without the
// CLI it shows the command, the portal and the T-SQL to do it by hand.

const props = defineProps<{ connectionId: string }>();
const emit = defineEmits<{ close: []; retry: [] }>();
const { t } = useTranslation();
const conns = useConnectionsStore();
const ui = useUiStore();

const conn = computed(() => conns.byId(props.connectionId));
const live = computed(() => conns.live[props.connectionId]);
const detail = computed(() => live.value?.detail ?? null);
const kind = computed<ConnectFailureKind>(() => detail.value?.kind ?? 'unknown');
const message = computed(() => live.value?.error ?? '');
const host = computed(() => conn.value?.config.host ?? '');
const port = computed(() => conn.value?.config.port || conns.driverOf(props.connectionId)?.default_port || 0);
const server = computed(() => (port.value ? `${host.value}:${port.value}` : host.value));
/** The host without `tcp:` and `,port`, lower case. */
const bareHost = computed(() => host.value.trim().toLowerCase().replace(/^tcp:/, '').split(/[,:\\/]/)[0].replace(/\.$/, ''));
const azureSql = computed(() => /^[a-z0-9-]+\.database\.(windows\.net|usgovcloudapi\.net|chinacloudapi\.cn)$/.test(bareHost.value));
/** SQL Server on an Azure VM: its NSG / WAF, not the Azure SQL firewall. */
const azureVm = computed(() => bareHost.value.endsWith('.cloudapp.azure.com'));
const retrying = computed(() => live.value?.status === 'connecting');
const network = computed(() => kind.value === 'timeout' || kind.value === 'refused');

// Connected after a retry: nothing left to explain.
watch(() => live.value?.status, (s) => { if (s === 'connected') emit('close'); });

// -- a pinned certificate that changed ---------------------------------------------------
const trusting = ref(false);
const trustError = ref<string | null>(null);
/** Forget the pinned certificate and reconnect: the server's current one
 *  gets pinned. Only on the user's click, after the message that names both
 *  fingerprints. */
async function trustNewCertificate() {
  trusting.value = true;
  trustError.value = null;
  try {
    await api.forgetTlsPin(props.connectionId);
    emit('retry');
  } catch (e) {
    trustError.value = errorMessage(e);
  } finally {
    trusting.value = false;
  }
}

// -- public IP (asked only on click) ----------------------------------------------------
const publicIp = ref<string | null>(null);
const ipLoading = ref(false);
const ipError = ref<string | null>(null);
async function lookUpIp() {
  ipLoading.value = true;
  ipError.value = null;
  try {
    publicIp.value = await api.publicIp();
    ruleIp.value = publicIp.value;
  } catch (e) {
    ipError.value = errorMessage(e);
  } finally {
    ipLoading.value = false;
  }
}
/** The IP the server saw (Azure says it), else the one looked up. */
const knownIp = computed(() => publicIp.value ?? detail.value?.client_ip ?? null);
/** The server named a different IP than this machine's: say so (it's unverified). */
const ipMismatch = computed(() => !!publicIp.value && !!detail.value?.client_ip && publicIp.value !== detail.value.client_ip);

// -- Azure SQL firewall rule ------------------------------------------------------------
const plan = ref<AzureFirewallPlan | null>(null);
const planning = ref(false);
const planError = ref<string | null>(null);
// Only this machine's own public IP (looked up over HTTPS), never the one
// the server's message names: an unencrypted connection's answer can come
// from anyone on the path. The backend checks it again before creating.
const ruleIp = ref('');
const prodTyped = ref('');
const prod = computed(() => isProdConnection(conn.value));
const creating = ref(false);
const createError = ref<string | null>(null);
const created = ref<string | null>(null);
const ipValid = computed(() => /^(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(\.(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/.test(ruleIp.value.trim()));
const canCreate = computed(() => !!plan.value && plan.value.az === 'ready' && ipValid.value && (!prod.value || prodTyped.value.trim() === plan.value.server) && !creating.value);

async function startAddRule() {
  planning.value = true;
  planError.value = null;
  created.value = null;
  try {
    if (!publicIp.value) await lookUpIp();
    if (!publicIp.value) return;
    plan.value = await api.azureFirewallPlan(props.connectionId);
  } catch (e) {
    planError.value = errorMessage(e);
  } finally {
    planning.value = false;
  }
}

async function createRule() {
  if (!plan.value || !canCreate.value) return;
  creating.value = true;
  createError.value = null;
  try {
    const r = await api.azureAddFirewallRule(props.connectionId, ruleIp.value.trim(), plan.value.rule_name);
    created.value = t('connection:problem.azure.created', { rule: r.rule_name, server: r.server, group: r.resource_group, ip: r.ip });
    plan.value = null;
  } catch (e) {
    createError.value = errorMessage(e);
  } finally {
    creating.value = false;
  }
}

/** By hand: the same rule with the CLI, and with T-SQL from another connection. */
// The manual instructions too: only the verified public IP, else a placeholder.
const manualIp = computed(() => (ipValid.value ? ruleIp.value.trim() : publicIp.value) ?? t('connection:problem.azure.ipPlaceholder'));
const cliCommand = computed(() => {
  const p = plan.value;
  if (!p) return '';
  return `az sql server firewall-rule create -g ${t('connection:problem.azure.groupPlaceholder')} -s ${p.server} -n ${p.rule_name} --start-ip-address ${manualIp.value} --end-ip-address ${manualIp.value}`;
});
const tsql = computed(() => {
  const p = plan.value;
  if (!p) return '';
  return `-- master\nEXECUTE sp_set_firewall_rule N'${p.rule_name}', '${manualIp.value}', '${manualIp.value}';`;
});
const PORTAL_URL = 'https://portal.azure.com/#browse/Microsoft.Sql%2Fservers';
function openPortal() {
  api.openAzurePortal().catch((e) => ElMessage.error(errorMessage(e)));
}

// -- footer ---------------------------------------------------------------------------
async function copy(text: string) {
  try {
    await navigator.clipboard.writeText(text);
    ElMessage.success({ message: t('common:copied'), duration: 1200 });
  } catch { /* no clipboard */ }
}

/** What support needs: no password, no other secret. */
function copyDetail() {
  const d = detail.value;
  const lines = [
    `${t('connection:problem.connection')}: ${conn.value?.name ?? props.connectionId}`,
    `${t('connection:problem.server')}: ${server.value} (${conn.value?.config.driver ?? ''})`,
    `${t('connection:problem.kindLabel')}: ${kind.value}${d?.code ? ` · ${t('connection:problem.code', { code: d.code })}` : ''}`,
    ...(knownIp.value ? [`${t('connection:problem.clientIp')}: ${knownIp.value}`] : []),
    `${t('connection:problem.message')}: ${message.value}`,
  ];
  void copy(lines.join('\n'));
}

function editConnection() {
  ui.editConnection(props.connectionId);
  emit('close');
}
</script>

<template>
  <el-dialog
    :model-value="true" width="640px" append-to-body :title="$t('connection:problem.title', { name: conn?.name ?? '' })"
    :close-on-click-modal="false" @close="emit('close')"
  >
    <div class="cp">
      <h4 class="cp-head">{{ $t(`connection:problem.kind.${kind}.title`) }}</h4>
      <p class="cp-server">{{ $t('connection:problem.server') }}: <code class="nm-selectable">{{ server }}</code></p>

      <!-- firewall -->
      <template v-if="kind === 'firewall'">
        <p>{{ azureSql ? $t('connection:problem.kind.firewall.azure') : $t('connection:problem.kind.firewall.other') }}</p>
      </template>
      <!-- timeout / refused -->
      <template v-else-if="network">
        <p>{{ $t(`connection:problem.kind.${kind}.body`) }}</p>
        <ul class="cp-list">
          <li>{{ $t('connection:problem.network.vpn') }}</li>
          <li>{{ $t('connection:problem.network.firewall', { port }) }}</li>
          <li>{{ $t('connection:problem.network.down') }}</li>
        </ul>
        <p v-if="azureVm" class="cp-note">{{ $t('connection:problem.network.azureVm', { host: bareHost }) }}</p>
        <p v-else-if="azureSql" class="cp-note">{{ $t('connection:problem.network.azureSql') }}</p>
      </template>
      <p v-else-if="kind === 'dns'">{{ $t('connection:problem.kind.dns.body', { host: bareHost }) }}</p>
      <p v-else-if="kind === 'tls' || kind === 'tls_not_offered' || kind === 'tls_untrusted'">{{ $t(`connection:problem.kind.${kind}.body`) }}</p>
      <!-- The pinned certificate changed: trust the new one (the next connect pins it). -->
      <template v-else-if="kind === 'tls_pin_changed'">
        <p>{{ $t('connection:problem.kind.tls_pin_changed.body') }}</p>
        <div class="cp-azure">
          <el-button type="warning" plain :loading="trusting" @click="trustNewCertificate">{{ $t('connection:problem.kind.tls_pin_changed.trust') }}</el-button>
          <div v-if="trustError" class="cp-error">{{ trustError }}</div>
        </div>
      </template>
      <p v-else-if="kind === 'auth'">{{ $t('connection:problem.kind.auth.body') }}</p>
      <template v-else>
        <p v-if="detail?.code === '40613'">{{ $t('connection:problem.azure.unavailable') }}</p>
        <p v-else>{{ $t('connection:problem.kind.unknown.body') }}</p>
      </template>

      <!-- the client's IP: what the server saw, or looked up on click -->
      <div v-if="kind === 'firewall' || network" class="cp-ip">
        <span v-if="publicIp">{{ $t('connection:problem.yourIp', { ip: publicIp }) }}</span>
        <span v-else-if="detail?.client_ip">{{ $t('connection:problem.ipFromServer', { ip: detail.client_ip }) }}</span>
        <el-button v-if="knownIp" :aria-label="$t('common:copy')" size="small" text :title="$t('common:copy')" @click="copy(knownIp)"><el-icon><ei-copy-document /></el-icon></el-button>
        <el-button v-if="!publicIp" size="small" :loading="ipLoading" @click="lookUpIp">{{ $t('connection:problem.publicIp') }}</el-button>
        <span v-if="ipMismatch" class="cp-error">{{ $t('connection:problem.ipMismatch', { server: detail?.client_ip }) }}</span>
        <span v-if="ipError" class="cp-error">{{ ipError }}</span>
      </div>

      <!-- Azure SQL: add this IP to the server's firewall -->
      <div v-if="kind === 'firewall' && azureSql" class="cp-azure">
        <el-button v-if="!plan && !created" type="primary" plain :loading="planning" @click="startAddRule">{{ $t('connection:problem.azure.add') }}</el-button>
        <div v-if="planError" class="cp-error">{{ planError }}</div>
        <el-alert v-if="created" type="success" :title="created" :closable="false" show-icon />

        <!-- confirmation: a change in the user's Azure account -->
        <div v-if="plan && plan.az === 'ready'" class="cp-confirm">
          <div class="cp-confirm-title">{{ $t('connection:problem.azure.confirmTitle') }}</div>
          <dl class="cp-facts">
            <dt>{{ $t('connection:problem.server') }}</dt><dd><code>{{ plan.server }}</code> <span class="nm-muted">({{ plan.host }})</span></dd>
            <dt>{{ $t('connection:problem.azure.rule') }}</dt><dd><code>{{ plan.rule_name }}</code></dd>
            <dt>{{ $t('connection:problem.azure.ip') }}</dt>
            <dd>
              <code>{{ ruleIp }}</code> <span class="nm-muted">{{ $t('connection:problem.azure.ipVerified') }}</span>
            </dd>
          </dl>
          <p class="cp-note">{{ $t('connection:problem.azure.confirmNote') }}</p>
          <div v-if="prod" class="cp-prod">
            <label for="cp-prod">{{ $t('connection:problem.azure.prodConfirm', { name: plan.server }) }}</label>
            <el-input id="cp-prod" v-model="prodTyped" size="small" :placeholder="plan.server" />
          </div>
          <div v-if="createError" class="cp-error">{{ createError }}</div>
          <div class="cp-actions">
            <el-button size="small" :disabled="creating" @click="plan = null">{{ $t('common:cancel') }}</el-button>
            <el-button size="small" type="danger" :loading="creating" :disabled="!canCreate" @click="createRule">{{ $t('connection:problem.azure.create') }}</el-button>
          </div>
        </div>

        <!-- no az, or not logged in: by hand -->
        <div v-else-if="plan" class="cp-manual">
          <p>{{ $t(plan.az === 'sandboxed' ? 'connection:problem.azure.azSandboxed' : plan.az === 'missing' ? 'connection:problem.azure.azMissing' : 'connection:problem.azure.azLoggedOut') }}</p>
          <div class="cp-way">
            <div class="cp-way-title">{{ $t('connection:problem.azure.byCli') }}</div>
            <pre class="cp-code nm-selectable">{{ cliCommand }}</pre>
            <el-button size="small" text @click="copy(cliCommand)">{{ $t('common:copy') }}</el-button>
          </div>
          <div class="cp-way">
            <div class="cp-way-title">{{ $t('connection:problem.azure.byPortal') }}</div>
            <pre class="cp-code nm-selectable">{{ PORTAL_URL }}</pre>
            <el-button size="small" text @click="openPortal">{{ $t('connection:problem.azure.openPortal') }}</el-button>
            <el-button size="small" text @click="copy(PORTAL_URL)">{{ $t('common:copy') }}</el-button>
          </div>
          <div class="cp-way">
            <div class="cp-way-title">{{ $t('connection:problem.azure.byTsql') }}</div>
            <pre class="cp-code nm-selectable">{{ tsql }}</pre>
            <el-button size="small" text @click="copy(tsql)">{{ $t('common:copy') }}</el-button>
          </div>
        </div>
      </div>

      <div class="cp-msg-head">
        {{ $t('connection:problem.message') }}<span v-if="detail?.code" class="nm-muted"> · {{ $t('connection:problem.code', { code: detail.code }) }}</span>
      </div>
      <pre class="cp-code cp-msg nm-selectable">{{ message }}</pre>
    </div>
    <template #footer>
      <div class="cp-foot">
        <el-button @click="copyDetail">{{ $t('connection:problem.copyDetail') }}</el-button>
        <el-button @click="editConnection">{{ $t('connection:problem.editConnection') }}</el-button>
        <span style="flex: 1" />
        <el-button @click="emit('close')">{{ $t('common:close') }}</el-button>
        <el-button type="primary" :loading="retrying" @click="emit('retry')">{{ $t('common:retry') }}</el-button>
      </div>
    </template>
  </el-dialog>
</template>

<style scoped lang="scss">
.cp { font-size: 13px; line-height: 1.5; }
.cp p { margin: 0 0 8px; }
.cp-head { margin: 0 0 6px; font-size: 14px; }
.cp-server code, .cp-facts code { font-family: var(--nm-mono, monospace); font-size: 12px; }
.cp-list { margin: 0 0 8px; padding-left: 18px; }
.cp-note { color: var(--nm-text-dim); font-size: 12.5px; }
.cp-ip { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; margin: 4px 0 10px; }
.cp-error { color: var(--nm-danger); font-size: 12.5px; }
.cp-azure { margin: 6px 0 12px; display: flex; flex-direction: column; gap: 8px; align-items: flex-start; }
.cp-confirm, .cp-manual { align-self: stretch; border: 1px solid var(--nm-border-soft); border-radius: 4px; padding: 10px 12px; }
.cp-confirm-title { font-weight: 600; margin-bottom: 6px; }
.cp-facts { display: grid; grid-template-columns: max-content 1fr; gap: 4px 12px; margin: 0 0 8px; align-items: center; }
.cp-facts dt { color: var(--nm-text-dim); }
.cp-facts dd { margin: 0; display: flex; align-items: center; gap: 8px; }
.cp-prod { display: flex; flex-direction: column; gap: 6px; margin-bottom: 8px; color: var(--nm-danger); font-size: 12.5px; }
.cp-actions { display: flex; justify-content: flex-end; gap: 8px; }
.cp-way { margin-top: 8px; }
.cp-way-title { font-size: 12.5px; color: var(--nm-text-dim); margin-bottom: 4px; }
.cp-code {
  margin: 0; padding: 6px 8px; border-radius: 3px; background: var(--ide-editor, rgba(0, 0, 0, .15));
  font-family: var(--nm-mono, monospace); font-size: 12px; white-space: pre-wrap; word-break: break-all;
}
.cp-msg-head { margin: 10px 0 4px; font-size: 12.5px; color: var(--nm-text-dim); }
.cp-msg { max-height: 160px; overflow: auto; }
.cp-foot { display: flex; gap: 8px; }
</style>
