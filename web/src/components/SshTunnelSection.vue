<script setup lang="ts">
import { open as openFile } from '@tauri-apps/plugin-dialog';

// The SSH tunnel of a connection (docs/tuneles-ssh.md): its settings go to
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
  /** Accepted server fingerprints, comma-separated. */
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

function forget() {
  model.value.trusted = '';
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
      <div v-if="model.trusted" class="st-trusted">
        <el-icon><ei-lock /></el-icon>
        <span>{{ $t('tunnel:trusted', { count: model.trusted.split(',').filter(Boolean).length }) }}</span>
        <el-button text size="small" @click="forget">{{ $t('tunnel:forget') }}</el-button>
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
.st-trusted { display: flex; align-items: center; gap: 6px; font-size: 12px; color: var(--nm-text); margin-bottom: 8px; }
.st-trusted .el-icon { color: var(--nm-success); }
</style>
