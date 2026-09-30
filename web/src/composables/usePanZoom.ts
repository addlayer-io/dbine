// Pan & zoom for a canvas of absolutely positioned content (plan trees, ER
// diagrams). The content lives in a "world" element transformed with
// `translate(view.x, view.y) scale(view.k)`, so panning never re-lays out
// anything. Drag the background to pan; plain wheel / two-finger scroll
// pans; pinch or ⌘/Ctrl + wheel zooms around the pointer. Until the user
// moves the view, it re-fits whenever the canvas resizes.
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref } from 'vue';

/** The world-space box the content occupies. */
export interface Bounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface PanZoomOptions {
  /** Content box in world coordinates (read on fit and for the minimap). */
  bounds: () => Bounds;
  /** Margin kept around the content when fitting. */
  pad?: number;
  minK?: number;
  maxK?: number;
  /** Largest zoom `fit()` picks (so a tiny diagram isn't blown up). */
  maxFitK?: number;
  /** Selector of elements where a pointerdown must not start a pan. */
  noPan?: string;
  /** Minimap size in px. */
  mini?: { w: number; h: number };
}

export function usePanZoom(opts: PanZoomOptions) {
  const pad = opts.pad ?? 40;
  const minK = opts.minK ?? 0.15;
  const maxK = opts.maxK ?? 2.5;
  const maxFitK = opts.maxFitK ?? 1.1;
  const MINI_W = opts.mini?.w ?? 184;
  const MINI_H = opts.mini?.h ?? 116;

  const vp = ref<HTMLElement | null>(null);
  const view = reactive({ x: pad, y: pad, k: 1 });
  const size = reactive({ w: 800, h: 400 });
  const panning = ref(false);

  /** Until the user pans or zooms, the content re-fits when the canvas resizes. */
  let userMoved = false;
  const markMoved = () => { userMoved = true; };
  const isUserMoved = () => userMoved;

  const clampK = (k: number) => Math.min(maxK, Math.max(minK, k));

  function zoomAt(k: number, cx: number, cy: number) {
    userMoved = true;
    const nk = clampK(k);
    view.x = cx - (cx - view.x) * (nk / view.k);
    view.y = cy - (cy - view.y) * (nk / view.k);
    view.k = nk;
  }
  function zoomBy(f: number) { zoomAt(view.k * f, size.w / 2, size.h / 2); }
  function actualSize() { zoomAt(1, size.w / 2, size.h / 2); }
  function panBy(dx: number, dy: number) {
    userMoved = true;
    view.x += dx;
    view.y += dy;
  }
  function fit() {
    userMoved = false;
    const b = opts.bounds();
    if (!b.width || !b.height) return;
    const k = Math.max(minK, Math.min(maxFitK, (size.w - pad * 2) / b.width, (size.h - pad * 2) / b.height));
    view.k = k;
    view.x = (size.w - b.width * k) / 2 - b.x * k;
    view.y = Math.max(pad / 2, (size.h - b.height * k) / 2) - b.y * k;
  }
  /** Puts a world point at the center of the canvas (optionally at zoom `k`). */
  function centerOn(wx: number, wy: number, k?: number) {
    userMoved = true;
    if (k !== undefined) view.k = clampK(k);
    view.x = size.w / 2 - wx * view.k;
    view.y = size.h / 2 - wy * view.k;
  }

  function onWheel(e: WheelEvent) {
    e.preventDefault();
    if (e.ctrlKey || e.metaKey) {
      // Pinch on a trackpad arrives as ctrl + wheel.
      const r = vp.value!.getBoundingClientRect();
      zoomAt(view.k * Math.exp(-e.deltaY * 0.0025), e.clientX - r.left, e.clientY - r.top);
    } else {
      panBy(-e.deltaX, -e.deltaY);
    }
  }

  function onPointerDown(e: PointerEvent) {
    if (e.button !== 0) return;
    if (opts.noPan && (e.target as HTMLElement).closest(opts.noPan)) return;
    panning.value = true;
    userMoved = true;
    const start = { x: e.clientX, y: e.clientY, vx: view.x, vy: view.y };
    const move = (ev: PointerEvent) => {
      view.x = start.vx + ev.clientX - start.x;
      view.y = start.vy + ev.clientY - start.y;
    };
    const up = () => {
      panning.value = false;
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  }

  /** F fits, 0 = 100 %, +/− zoom. Returns whether it handled the key. */
  function onKey(e: KeyboardEvent): boolean {
    if (e.metaKey || e.ctrlKey || e.altKey) return false;
    const k = e.key.toLowerCase();
    if (k === 'f') fit();
    else if (k === '0') actualSize();
    else if (k === '+' || k === '=') zoomBy(1.2);
    else if (k === '-') zoomBy(1 / 1.2);
    else return false;
    e.preventDefault();
    return true;
  }

  let ro: ResizeObserver | null = null;
  onMounted(() => {
    ro = new ResizeObserver(() => {
      const r = vp.value?.getBoundingClientRect();
      if (!r) return;
      // Once the user has moved, a resize (a side panel opening…) keeps
      // what was at the center of the canvas at the center.
      if (userMoved) {
        view.x += (r.width - size.w) / 2;
        view.y += (r.height - size.h) / 2;
      }
      size.w = r.width;
      size.h = r.height;
      if (!userMoved) fit();
    });
    if (vp.value) ro.observe(vp.value);
    nextTick(fit);
  });
  onBeforeUnmount(() => ro?.disconnect());

  // -- minimap -------------------------------------------------------------------------------
  /** World → minimap: `mx = ox + (wx - bx) * s`. */
  const mini = computed(() => {
    const b = opts.bounds();
    const s = Math.min((MINI_W - 12) / Math.max(b.width, 1), (MINI_H - 12) / Math.max(b.height, 1));
    return { w: MINI_W, h: MINI_H, s, bx: b.x, by: b.y, ox: (MINI_W - b.width * s) / 2, oy: (MINI_H - b.height * s) / 2 };
  });
  /** The visible part of the world, as a frame clipped to the minimap. */
  const miniView = computed(() => {
    const m = mini.value;
    const x = m.ox + (-view.x / view.k - m.bx) * m.s;
    const y = m.oy + (-view.y / view.k - m.by) * m.s;
    const x1 = Math.max(1, x);
    const y1 = Math.max(1, y);
    const x2 = Math.min(MINI_W - 1, x + (size.w / view.k) * m.s);
    const y2 = Math.min(MINI_H - 1, y + (size.h / view.k) * m.s);
    return { x: x1, y: y1, w: Math.max(0, x2 - x1), h: Math.max(0, y2 - y1) };
  });
  function miniPoint(clientX: number, clientY: number, el: HTMLElement) {
    const r = el.getBoundingClientRect();
    const m = mini.value;
    centerOn(m.bx + (clientX - r.left - m.ox) / m.s, m.by + (clientY - r.top - m.oy) / m.s);
  }
  function onMiniDown(e: PointerEvent) {
    const el = e.currentTarget as HTMLElement;
    miniPoint(e.clientX, e.clientY, el);
    const move = (ev: PointerEvent) => miniPoint(ev.clientX, ev.clientY, el);
    const up = () => { window.removeEventListener('pointermove', move); window.removeEventListener('pointerup', up); };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  }

  return {
    vp, view, size, panning,
    zoomAt, zoomBy, actualSize, panBy, fit, centerOn,
    onWheel, onPointerDown, onKey,
    mini, miniView, onMiniDown,
    markMoved, isUserMoved,
  };
}
