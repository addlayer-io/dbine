<script setup lang="ts">
// Text whose web addresses are links: a click opens the address in the system's browser, never
// inside DBine (`openExternalLink`, http and https only).
import { computed } from 'vue';
import { linkParts } from '../composables/linkParts';
import { openExternalLink } from '../composables/externalLink';

const props = defineProps<{ text: string }>();
const parts = computed(() => linkParts(props.text));
</script>

<template>
  <span class="lt"><template v-for="(p, i) in parts" :key="i"><a v-if="p.url" class="lt-link" :href="p.url" :title="p.url" @click.prevent.stop="openExternalLink(p.url)">{{ p.text }}</a><template v-else>{{ p.text }}</template></template></span>
</template>

<style scoped>
.lt-link { color: var(--el-color-primary); text-decoration: underline; cursor: pointer; word-break: break-all; }
.lt-link:hover { color: var(--el-color-primary-light-3); }
</style>
