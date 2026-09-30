import type { SqlLanguage } from 'sql-formatter';
import type { Language } from '../api/types';
import { rt } from './i18nLabels';

// "Formatear" in the query editor, per language. The formatters load on first
// use (dynamic import), so they don't weigh on the app's start.
// - SQL (and CQL): sql-formatter, with the driver's dialect.
// - JSON / MongoDB shell: JSON when it parses, else js-beautify.
// - Redis, Flux, Cypher: no formatter (the button says why).

/** Why a language can't be formatted, or null when it can. */
export function formatUnavailable(language: Language | undefined): string | null {
  switch (language ?? 'sql') {
    case 'sql':
    case 'cql':
    case 'json':
      return null;
    case 'redis':
      return rt('core:format.redis');
    default:
      return rt('core:format.unavailable');
  }
}

/** DBine's dialect ids → sql-formatter's languages ('sql' for the rest). */
const SQL_LANGUAGES: Record<string, SqlLanguage> = {
  mssql: 'transactsql',
  sybase: 'transactsql',
  postgres: 'postgresql',
  mysql: 'mysql',
  mariadb: 'mariadb',
  sqlite: 'sqlite',
  oracle: 'plsql',
  db2: 'db2',
  hive: 'hive',
  sparksql: 'spark',
  databricks: 'spark',
  trino: 'trino',
  snowflake: 'snowflake',
  bigquery: 'bigquery',
  redshift: 'redshift',
  n1ql: 'n1ql',
  duckdb: 'duckdb',
  tidb: 'tidb',
  singlestore: 'singlestoredb',
  clickhouse: 'clickhouse',
};

/** `{{parámetro}}` of Library scripts stays as one token. */
const PARAM = String.raw`\{\{[^{}]+\}\}`;

async function formatSql(text: string, dialect: string): Promise<string> {
  const { format } = await import('sql-formatter');
  const language: SqlLanguage = SQL_LANGUAGES[dialect] ?? 'sql';
  const one = (sql: string) =>
    format(sql, {
      language,
      tabWidth: 2,
      keywordCase: 'preserve',
      linesBetweenQueries: 1,
      paramTypes: { custom: [{ regex: PARAM }] },
    });
  // T-SQL batches: `GO` is a client separator, not SQL; each batch alone.
  if (language === 'transactsql' && /^\s*GO\s*$/im.test(text)) {
    return text
      .split(/^\s*GO\s*$/im)
      .map((b) => b.trim())
      .filter(Boolean)
      .map(one)
      .join('\nGO\n\n')
      .concat('\nGO\n');
  }
  return one(text);
}

async function formatJs(text: string): Promise<string> {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    // MongoDB shell: JavaScript.
    const { js } = await import('js-beautify');
    return js(text, { indent_size: 2, brace_style: 'collapse', end_with_newline: false });
  }
}

/** The text formatted for its language; throws with the reason when it can't. */
export async function formatCode(text: string, language: Language | undefined, dialect: string): Promise<string> {
  const lang = language ?? 'sql';
  const why = formatUnavailable(lang);
  if (why) throw new Error(why);
  if (!text.trim()) return text;
  return lang === 'json' ? formatJs(text) : formatSql(text, lang === 'cql' ? '' : dialect);
}
