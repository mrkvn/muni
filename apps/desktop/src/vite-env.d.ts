/// <reference types="vite/client" />

interface ImportMetaEnv {
  /**
   * Feature 033 — frontend Sentry DSN, baked at build time (prod only; empty in
   * dev/staging so the webview SDK no-ops). Forwarded by `vite.config.ts` from
   * the prod build's `VITE_SENTRY_DSN` env (see `scripts/_prod-build-env.sh`).
   */
  readonly VITE_SENTRY_DSN: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
