<script setup lang="ts">
import { computed } from 'vue';
import type { DocOptions } from '../api/dbDocs';

// The "Documentar la base" step of a scheduled task: format, schemas and
// what to include, as the dialog has them (DbDocsDialog.vue). Parts the
// database lacks just come out empty. Edits `config.options` in place.

const props = defineProps<{ config: Record<string, any> }>();
const emit = defineEmits<{ change: [] }>();

const o = computed(() => props.config.options as DocOptions);

const PARTS = ['tables', 'views', 'routines', 'triggers', 'others', 'source', 'indexes', 'foreign_keys', 'dependencies', 'diagram'] as const;
const partLabel = (key: string) => `dbDocs:parts.${key === 'foreign_keys' ? 'foreignKeys' : key}`;
</script>

<template>
  <div class="sd">
    <el-form-item :label="$t('dbDocs:format')">
      <el-radio-group v-model="o.format" @change="emit('change')">
        <el-radio value="html">{{ $t('dbDocs:formats.html') }}</el-radio>
        <el-radio value="markdown">{{ $t('dbDocs:formats.markdown') }}</el-radio>
      </el-radio-group>
    </el-form-item>
    <el-form-item :label="$t('dbDocs:schemas')">
      <el-select
        v-model="o.schemas" multiple filterable allow-create default-first-option :reserve-keyword="false"
        :placeholder="$t('dbDocs:allSchemas')" class="sd-schemas" @change="emit('change')"
      />
    </el-form-item>
    <el-form-item :label="$t('dbDocs:include')">
      <div class="sd-parts">
        <el-checkbox
          v-for="p in PARTS" :key="p" v-model="(o[p] as boolean)" :disabled="p === 'diagram' && o.format !== 'html'"
          @change="emit('change')"
        >{{ $t(partLabel(p)) }}</el-checkbox>
      </div>
    </el-form-item>
  </div>
</template>

<style scoped>
.sd-schemas { width: 100%; max-width: 520px; }
.sd-parts { display: grid; grid-template-columns: repeat(auto-fill, minmax(230px, 1fr)); gap: 0 12px; width: 100%; }
.sd-parts .el-checkbox { margin-right: 0; }
</style>
