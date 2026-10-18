// The Element Plus icon (without its `ei-` prefix) of each object kind, as
// the explorer shows it; "Ir a…" shows the same.

export const KIND_ICONS: Record<string, string> = {
  table: 'grid', view: 'view', materialized_view: 'copy-document', procedure: 'operation',
  function: 'cpu', trigger: 'lightning', sequence: 'sort', collection: 'files', key: 'key',
  index: 'collection', topic: 'chat-line-square', stream: 'share', measurement: 'trend-charts',
  package: 'box', alias: 'link', type: 'price-tag', device: 'monitor', dictionary: 'notebook',
  constraint: 'lock', supertable: 'menu', subtable: 'grid', task: 'timer', file: 'document',
  label: 'coordinate', vertex: 'coordinate', relationship: 'connection', edge: 'connection',
  source: 'upload', sink: 'download', synonym: 'link', domain: 'price-tag', virtual_table: 'grid',
  fulltext_catalog: 'reading', fulltext_stoplist: 'document-remove',
};
