declare module '*.vue' {
  import type { DefineComponent } from 'vue';
  const component: DefineComponent<{}, {}, any>;
  export default component;
}

// Leaflet 1.9 ships no types (and only `leaflet` is a dependency): MapView.vue
// is its one user.
declare module 'leaflet' {
  const L: any;
  export = L;
}
