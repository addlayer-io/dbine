import { onBeforeUnmount, onMounted, onUpdated, ref, type Ref } from 'vue';

// A toolbar whose actions never go off-screen (VS Code's "…"): the items
// that don't fit, last first, are taken out of the line and offered in an
// overflow menu.
//
// Markup: the toolbar's direct children marked `data-ov="<key>"` can
// collapse, in the order they appear; `data-ov-fixed` ones always stay; the
// "…" button is `data-ov-more`. A collapsed item stays in the DOM, out of
// the flow and invisible (so it can still be measured, and a popover can
// still anchor to it): the caller gives it the class for `hidden`.

export function useToolbarOverflow(bar: Ref<HTMLElement | null>) {
  /** Keys of the collapsed items. */
  const hidden = ref<string[]>([]);
  let observer: ResizeObserver | null = null;
  const watched = new Set<Element>();

  function layout() {
    const el = bar.value;
    if (!el) return;
    const css = getComputedStyle(el);
    const gap = parseFloat(css.columnGap) || 0;
    const room = el.clientWidth - (parseFloat(css.paddingLeft) || 0) - (parseFloat(css.paddingRight) || 0);
    const children = [...el.children] as HTMLElement[];
    const fixed = children.filter((c) => c.hasAttribute('data-ov-fixed'));
    const items = children.filter((c) => c.hasAttribute('data-ov'));
    const more = children.find((c) => c.hasAttribute('data-ov-more'));
    const width = (c: HTMLElement) => c.getBoundingClientRect().width;
    // Anything else in the line (a spacer) only takes its gap.
    const others = children.filter((c) => !c.hasAttribute('data-ov') && !c.hasAttribute('data-ov-fixed') && !c.hasAttribute('data-ov-more')
      && getComputedStyle(c).position !== 'absolute');
    let used = fixed.reduce((s, c) => s + width(c) + gap, 0) + others.length * gap;
    const widths = items.map(width);
    const total = widths.reduce((s, w) => s + w + gap, 0);
    let fit = items.length;
    if (used + total - gap > room) {
      // Not all of them: leave room for "…".
      used += (more ? width(more) : 28) + gap;
      fit = 0;
      while (fit < items.length && used + widths[fit] <= room) {
        used += widths[fit] + gap;
        fit++;
      }
    }
    const next = items.slice(fit).map((c) => c.dataset.ov ?? '');
    if (next.join('\u0000') !== hidden.value.join('\u0000')) hidden.value = next;
    observe(children);
  }

  /** Re-measure when an item changes size (a label hidden, a spinner). */
  function observe(children: Element[]) {
    if (!observer) return;
    for (const c of children) {
      if (!watched.has(c)) { watched.add(c); observer.observe(c); }
    }
    for (const c of [...watched]) {
      if (!c.isConnected) { watched.delete(c); observer.unobserve(c); }
    }
  }

  let frame = 0;
  function schedule() {
    if (frame) return;
    frame = requestAnimationFrame(() => { frame = 0; layout(); });
  }

  onMounted(() => {
    observer = new ResizeObserver(schedule);
    if (bar.value) observer.observe(bar.value);
    layout();
  });
  // Items come and go (running, the driver's capabilities).
  onUpdated(schedule);
  onBeforeUnmount(() => {
    observer?.disconnect();
    if (frame) cancelAnimationFrame(frame);
  });

  return { hidden, isHidden: (key: string) => hidden.value.includes(key) };
}
