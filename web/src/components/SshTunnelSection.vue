<script setup lang="ts">
import { computed } from 'vue';
import { open as openFile } from '@tauri-apps/plugin-dialog';
import { forgetTrusted, parseTrusted } from '../composables/sshTrust';

// The SSH tunnel of a connection (docs/ssh-tunnels.md): its settings go to
// the connection's options as `ssh.*`; the password and the key's passphrase
// are secrets (keychain), never read back.

export interface SshValues {
  enabled: boolean;
  host: string;
  port: string;
  user: string;
  auth: 'password' | 'key' | 'agent';
  password: string;
  key_path: string;
  passphrase: string;
  /** Jump hosts, `user@host:port` separated by commas. */
  jump: string;
  /** Accepted server keys, `[host]:port SHA256:…` each, comma-separated. */
  trusted: string;
}

const model = defineModel<SshValues>({ required: true });
defineProps<{ editing: boolean }>();

async function browseKey() {
  try {
    const p = await openFile({ multiple: false, directory: false });
    if (typeof p === 'string') model.value.key_path = p;
  } catch { /* no dialog outside Tauri */ }
}

// The servers whose key the user accepted. Forgetting one is how a server
// that changed its key is accepted again: the next connection asks for it.
const trusted = computed(() =>
  model.value.trusted
    .split(',')
    .map((e) => e.trim())
    .filter(Boolean)
    .map((e, i) => ({ i, entry: parseTrusted(e) })),
);

function forget(i: number) {
  model.value.trusted = forgetTrusted(model.value.trusted, i);
}
</script>

<template>
  <section class="st" :class="{ on: model.enabled }">
    <header class="st-head">
      <el-switch v-model="model.enabled" size="small" />
      <div>
        <strong>{{ $t('tunnel:title') }}</strong>
        <span>{{ $t('tunnel:subtitle') }}</span>
      </div>
    </header>
    <div v-if="model.enabled" class="st-body">
      <div class="st-row">
        <el-form-item :label="$t('tunnel:host')" required class="st-grow">
          <el-input v-model="model.host" :placeholder="$t('tunnel:hostPlaceholder')" />
        </el-form-item>
        <el-form-item :label="$t('tunnel:port')" class="st-port">
          <el-input v-model="model.port" placeholder="22" />
        </el-form-item>
      </div>
      <el-form-item :label="$t('tunnel:user')" required>
        <el-input v-model="model.user" />
      </el-form-item>
      <el-form-item :label="$t('tunnel:auth')">
        <el-radio-group v-model="model.auth" size="small">
          <el-radio-button value="password">{{ $t('tunnel:authPassword') }}</el-radio-button>
          <el-radio-button value="key">{{ $t('tunnel:authKey') }}</el-radio-button>
          <el-radio-button value="agent">{{ $t('tunnel:authAgent') }}</el-radio-button>
        </el-radio-group>
      </el-form-item>
      <el-form-item v-if="model.auth === 'password'" :label="$t('tunnel:password')">
        <el-input v-model="model.password" type="password" show-password :placeholder="editing ? $t('tunnel:secretKept') : ''" />
      </el-form-item>
      <template v-else-if="model.auth === 'key'">
        <el-form-item :label="$t('tunnel:keyPath')" required>
          <el-input v-model="model.key_path" placeholder="~/.ssh/id_ed25519">
            <template #append>
              <el-button @click="browseKey"><el-icon><ei-folder-opened /></el-icon></el-button>
            </template>
          </el-input>
        </el-form-item>
        <el-form-item :label="$t('tunnel:passphrase')">
          <el-input v-model="model.passphrase" type="password" show-password :placeholder="editing ? $t('tunnel:secretKept') : $t('tunnel:passphrasePlaceholder')" />
        </el-form-item>
      </template>
      <div v-else class="st-help">{{ $t('tunnel:agentHelp') }}</div>
      <el-form-item :label="$t('tunnel:jump')">
        <el-input v-model="model.jump" :placeholder="$t('tunnel:jumpPlaceholder')" />
        <div class="st-help">{{ $t('tunnel:jumpHelp') }}</div>
      </el-form-item>
      <div v-if="trusted.length" class="st-trusted">
        <div class="st-trusted-head">
          <el-icon><ei-lock /></el-icon>
          <span>{{ $t('tunnel:trusted', { count: trusted.length }) }}</span>
        </div>
        <div v-for="t in trusted" :key="t.i" class="st-trusted-row">
          <template v-if="t.entry">
            <span class="st-server">{{ t.entry.host }}:{{ t.entry.port }}</span>
            <code :title="t.entry.fingerprint">{{ t.entry.fingerprint }}</code>
          </template>
          <span v-else class="st-server st-legacy">{{ $t('tunnel:trustedLegacy') }}</span>
          <el-button text size="small" @click="forget(t.i)">{{ $t('tunnel:forget') }}</el-button>
        </div>
        <div class="st-help">{{ $t('tunnel:trustedHelp') }}</div>
      </div>
      <div class="st-help">{{ $t('tunnel:tlsNote') }}</div>
    </div>
  </section>
</template>

<style scoped>
.st { border: 1px solid var(--nm-border-soft); border-radius: 4px; background: var(--nm-bg-elev); }
.st-head { display: flex; align-items: center; gap: 10px; padding: 10px 12px; }
.st-head > div { display: flex; flex-direction: column; line-height: 1.3; min-width: 0; }
.st-head strong { color: var(--nm-text-strong); font-weight: 600; }
.st-head span { font-size: 11.5px; color: var(--nm-text-dim); }
.st-body { padding: 4px 12px 10px; border-top: 1px solid var(--nm-border-soft); }
.st-row { display: flex; gap: 10px; }
.st-grow { flex: 1; min-width: 0; }
.st-port { width: 84px; flex: none; }
.st-help { font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.4; margin: 2px 0 10px; }
.st-trusted { font-size: 12px; color: var(--nm-text); margin-bottom: 8px; }
.st-trusted-head { display: flex; align-items: center; gap: 6px; margin-bottom: 2px; }
.st-trusted-head .el-icon { color: var(--nm-success); }
.st-trusted-row { display: flex; align-items: center; gap: 8px; padding-left: 20px; min-width: 0; }
.st-server { flex: none; color: var(--nm-text-strong); }
.st-legacy { color: var(--nm-text-dim); flex: 1; }
.st-trusted-row code { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--nm-text-dim); font-size: 11px; }
.st-trusted .st-help { padding-left: 20px; margin-bottom: 0; }
</style>
