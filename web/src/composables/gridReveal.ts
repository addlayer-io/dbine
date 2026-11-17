// Where a results grid scrolls so the cell a key moved to shows whole: the
// least scroll that brings it into the part of the view its sticky header
// rows and row-number column don't cover, up and down and side to side.
// No Vue here: a node test checks it.

export interface ScrollView {
  /** Current scroll. */
  top: number;
  left: number;
  /** The view's size (scrollbars aside). */
  height: number;
  width: number;
}

/** A cell's place in the scrolled content, measured from below the sticky
 *  header rows and right of the sticky row-number column. */
export interface CellBox {
  top: number;
  height: number;
  left: number;
  width: number;
}

/** The scroll (`top`, `left`) that shows `cell`. `stickyTop` / `stickyLeft`:
 *  what stays over the view's top and left edges. A cell bigger than the
 *  view lines up with its top or left edge. */
export function revealCell(view: ScrollView, cell: CellBox, stickyTop: number, stickyLeft: number): { top: number; left: number } {
  const axis = (scroll: number, size: number, start: number, length: number) => {
    if (start < scroll) return start;
    if (start + length > scroll + size) return Math.max(0, Math.min(start, start + length - size));
    return scroll;
  };
  return {
    top: axis(view.top, view.height - stickyTop, cell.top, cell.height),
    left: axis(view.left, view.width - stickyLeft, cell.left, cell.width),
  };
}
