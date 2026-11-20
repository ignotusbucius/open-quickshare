/// <reference types="vite/client" />

// Stylesheets imported for their side effects (TS 6 checks those imports).
declare module '*.postcss'

declare module '*.vue' {
  import type { DefineComponent } from 'vue'
  const component: DefineComponent<object, object, unknown>
  export default component
}
