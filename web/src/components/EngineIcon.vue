<script setup lang="ts">
import { computed } from 'vue';
import { ENGINE_ICONS } from '../assets/engine-icons';

// An engine's logo, or a colored badge with its initials when we have none.

const props = withDefaults(defineProps<{ id: string; name: string; size?: number }>(), { size: 20 });

/** Driver id → icon in ENGINE_ICONS. Engines built on another one's
 *  technology, or sold by the same vendor, share its logo. */
const ICON_OF: Record<string, string> = {
  postgres: 'postgresql', edb: 'postgresql', fujitsu: 'fujitsu', gaussdb: 'huawei', opengauss: 'huawei',
  cockroachdb: 'cockroachlabs', timescaledb: 'timescale', yugabytedb: 'yugabytedb', supabase: 'supabase', neon: 'neon',
  alloydb: 'googlecloud', cloudsql_postgres: 'googlecloud', cloudsql_mysql: 'googlecloud', cloudsql: 'googlecloud',
  redshift: 'amazonwebservices', aurora_postgres: 'amazonwebservices', aurora_mysql: 'amazonwebservices',
  dsql: 'amazonwebservices', athena: 'amazonwebservices', documentdb: 'amazonwebservices',
  keyspaces: 'amazonwebservices', neptune: 'amazonwebservices', timestream: 'amazonwebservices',
  dynamodb: 'dynamodb', cosmosdb: 'cosmosdb', azuresql: 'azuresqldatabase', fabric: 'azure', azure_databricks: 'databricks',
  mysql: 'mysql', mariadb: 'mariadb', tidb: 'tidb', singlestore: 'singlestore', doris: 'apachedoris', velodb: 'apachedoris',
  planetscale: 'planetscale',
  sqlserver: 'microsoftsqlserver', babelfish: 'amazonwebservices',
  oracle: 'oracle', oracle_autonomous: 'oracle', sqlite: 'sqlite', duckdb: 'duckdb', duckdb_files: 'duckdb',
  libsql: 'turso', turso: 'turso', firebird: 'firebird', h2: 'h2database',
  mongodb: 'mongodb', ferretdb: 'ferretdb', couchdb: 'couchdb', couchbase: 'couchbase',
  redis: 'redis', etcd: 'etcd',
  cassandra: 'cassandra', scylladb: 'scylladb',
  elasticsearch: 'elasticsearch', opensearch: 'opensearch', opendistro: 'opensearch', solr: 'apachesolr',
  clickhouse: 'clickhouse', influxdb: 'influxdb', influxdb1: 'influxdb', influxdb3: 'influxdb',
  trino: 'trino', starburst: 'trino', presto: 'presto', bigquery: 'googlebigquery', spanner: 'googlecloudspanner',
  snowflake: 'snowflake', databricks: 'databricks', hive: 'apachehive', spark: 'apachespark', spark_thrift: 'apachespark',
  kyuubi: 'apachespark', cloudera: 'cloudera', impala: 'cloudera', phoenix: 'apachehbase', ksqldb: 'apachekafka',
  kylin: 'apachekylin', flightsql: 'apachearrow', gizmosql: 'apachearrow',
  hana: 'sap', sybase: 'sap', sqlanywhere: 'sap', maxdb: 'sap', teradata: 'teradata', cratedb: 'cratedb',
  neo4j: 'neo4j', openedge: 'progress',
  // ids as the drivers declare them
  'aurora-mysql': 'amazonwebservices', 'cloudsql-mysql': 'googlecloud', oracle_adb: 'oracle',
  sparksql: 'apachespark',
};

const src = computed(() => {
  const key = ICON_OF[props.id] ?? props.id;
  return ENGINE_ICONS[key] ?? null;
});

// Badge: a stable hue per engine, so the same engine keeps its color.
const hue = computed(() => [...props.id].reduce((h, c) => (h * 31 + c.charCodeAt(0)) % 360, 7));
const initials = computed(() => {
  const words = props.name.replace(/\(.*?\)/g, '').split(/[\s/-]+/).filter(Boolean);
  return (words.length > 1 ? words[0][0] + words[1][0] : props.name.slice(0, 2)).toUpperCase();
});
</script>

<template>
  <img
    v-if="src"
    :src="src"
    :alt="name"
    class="ei"
    :style="{ width: `${size}px`, height: `${size}px`, padding: `${Math.max(1, Math.round(size * 0.12))}px` }"
    draggable="false"
  />
  <span
    v-else
    class="ei ei-badge"
    :style="{ width: `${size}px`, height: `${size}px`, fontSize: `${Math.round(size * 0.45)}px`, '--h': hue }"
    aria-hidden="true"
  >{{ initials }}</span>
</template>

<style scoped>
.ei { flex-shrink: 0; display: inline-block; box-sizing: border-box; border-radius: 4px; }
/* Logos sit on a light tile: many brand marks are dark and vanish on the
   dark theme otherwise. */
img.ei { object-fit: contain; background: #f3f3f3; }
.ei-badge {
  display: inline-flex; align-items: center; justify-content: center; border-radius: 4px;
  font-weight: 700; letter-spacing: -0.02em; color: #fff;
  background: hsl(var(--h) 45% 38%);
}
</style>
