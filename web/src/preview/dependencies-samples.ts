import type { DependencyReport } from '../api/types';

// view=dependencies: what a SQL Server database answers for dbo.Clientes.Pepe
// (the same objects as the driver's live test).
export const sampleDependents: DependencyReport = {
  scanned: 5,
  unreadable: ['dbo.pCifrado'],
  note: null,
  items: [
    { kind: 'table', schema: 'dbo', name: 'Clientes', parent: null, relation: 'index', confidence: 'confirmed', detail: 'IX_Pepe (Pepe)', mentions: [] },
    { kind: 'table', schema: 'dbo', name: 'Clientes', parent: null, relation: 'check', confidence: 'confirmed', detail: 'CK_Pepe: ([Pepe]>(0))', mentions: [] },
    {
      kind: 'view', schema: 'dbo', name: 'vPepe', parent: null, relation: 'code', confidence: 'confirmed', detail: null,
      mentions: [{ line: 1, text: 'CREATE VIEW dbo.vPepe WITH SCHEMABINDING AS SELECT c.id, c.Pepe FROM dbo.Clientes c', dynamic: false }],
    },
    {
      kind: 'procedure', schema: 'dbo', name: 'pPepe', parent: null, relation: 'code', confidence: 'probable', detail: null,
      mentions: [{ line: 3, text: 'SELECT Pepe', dynamic: false }, { line: 7, text: 'UPDATE dbo.Clientes SET Pepe = Pepe + 1 WHERE id = @id', dynamic: false }],
    },
    {
      kind: 'trigger', schema: 'dbo', name: 'trClientesAudit', parent: 'Clientes', relation: 'code', confidence: 'probable', detail: null,
      mentions: [{ line: 5, text: 'INSERT INTO audit.Cambios (tabla, valor) SELECT \'Clientes\', i.Pepe FROM inserted i', dynamic: false }],
    },
    {
      kind: 'procedure', schema: 'dbo', name: 'pDinamico', parent: null, relation: 'code', confidence: 'review', detail: null,
      mentions: [{ line: 1, text: "CREATE PROCEDURE dbo.pDinamico AS EXEC sp_executesql N'SELECT Pepe FROM dbo.Clientes'", dynamic: true }],
    },
  ],
};
