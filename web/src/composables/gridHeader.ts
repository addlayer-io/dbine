// The results grid's header rows: the column names, with each column's data
// type in small text under its name when shown, and the filter row. The
// names row grows to fit the types; the data rows start below both.
// No Vue here: a node test checks it.

/** Setting (Configuración › General): each column's data type under its
 *  name in the grids' header. On unless turned off. */
export const GRID_TYPES_KEY = 'grid.columnTypes';

/** The names row with the types is this many data rows tall. */
const TYPES_ROW = 1.6;

/** Whether the names row shows the types: the setting is on and the engine
 *  gave some column a type (engines without types keep the one-line header). */
export function showsTypes(columns: { type_name?: string | null }[], on: boolean): boolean {
  return on && columns.some((c) => !!c.type_name);
}

/** The names row's height, and where the data rows start: below the names
 *  row and, when the grid has one, the filter row (a data row tall). */
export function headerLayout(rowH: number, types: boolean, filterable: boolean): { names: number; top: number } {
  const names = types ? Math.round(rowH * TYPES_ROW) : rowH;
  return { names, top: names + (filterable ? rowH : 0) };
}
