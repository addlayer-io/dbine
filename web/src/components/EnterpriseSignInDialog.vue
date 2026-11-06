<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorKind, errorMessage } from '../api/client';
import { enterpriseApi } from '../api/enterprise';
import { useEnterpriseStore } from '../stores/enterprise';

// Signing in to DBine Enterprise: the company's server, then its SSO in the
// browser. Signed in, who and where, and signing out.

const ent = useEnterpriseStore();
const { t } = useTranslation();
const server = ref('');
const waiting = ref(false);
const busy = ref(false);
const status = computed(() => ent.status);

watch(() => ent.signInOpen, (open) => {
  if (open) server.value = ent.status?.server ?? '';
});

async function signIn() {
  if (!server.value.trim()) return;
  waiting.value = true;
  try {
    await ent.signIn(server.value);
    ElMessage.success(t('enterprise:signIn.done', { organization: ent.organization }));
    ent.signInOpen = false;
  } catch (e) {
    if (errorKind(e) !== 'cancelled') ElMessage.error(errorMessage(e));
  } finally {
    waiting.value = false;
  }
}

async function cancel() {
  await enterpriseApi.cancelSignIn().catch(() => {});
}

async function sync() {
  busy.value = true;
  try {
    await ent.sync();
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    busy.value = false;
  }
}

async function signOut() {
  busy.value = true;
  try {
    await ent.signOut();
    ent.signInOpen = false;
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <el-dialog
    :model-value="ent.signInOpen"
    :title="$t('enterprise:signIn.title')"
    width="460px"
    append-to-body
    @close="waiting ? cancel() : null; ent.signInOpen = false"
  >
    <template v-if="status?.signed_in">
      <p class="es-who">
        <strong>{{ ent.organization }}</strong><br>
        <span class="nm-muted">{{ $t('enterprise:signIn.as', { name: status.name || status.email, email: status.email }) }}</span><br>
        <span class="nm-muted mono">{{ status.server }}</span>
      </p>
      <p v-if="!status.online" class="es-offline">{{ $t('enterprise:offline') }}<template v-if="status.error"> ({{ status.error }})</template></p>
    </template>
    <el-form v-else label-position="top" @submit.prevent="signIn">
      <p class="nm-muted es-intro">{{ $t('enterprise:signIn.intro') }}</p>
      <el-form-item :label="$t('enterprise:signIn.server')">
        <el-input v-model="server" :placeholder="$t('enterprise:signIn.serverPlaceholder')" :disabled="waiting" autofocus @keyup.enter="signIn" />
      </el-form-item>
      <p v-if="waiting" class="nm-muted">{{ $t('enterprise:signIn.waiting') }}</p>
    </el-form>
    <template #footer>
      <template v-if="status?.signed_in">
        <el-button :loading="busy" @click="sync">{{ $t('enterprise:sync') }}</el-button>
        <el-button type="danger" plain :disabled="busy" @click="signOut">{{ $t('enterprise:signOut') }}</el-button>
      </template>
      <template v-else>
        <el-button v-if="waiting" @click="cancel">{{ $t('common:cancel') }}</el-button>
        <el-button v-else @click="ent.signInOpen = false">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" :loading="waiting" :disabled="!server.trim()" @click="signIn">{{ $t('enterprise:signIn.button') }}</el-button>
      </template>
    </template>
  </el-dialog>
</template>

<style scoped>
.es-intro {
  margin: 0 0 12px;
  line-height: 1.45;
}

.es-who {
  line-height: 1.6;
  margin: 0;
}

.es-offline {
  color: var(--el-color-warning);
}
</style>
