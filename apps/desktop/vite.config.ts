import { defineConfig } from "vite";
import { resolve } from "node:path";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;
// @ts-expect-error process is a nodejs global
const devPort = Number(process.env.MUNI_DEV_PORT) || 1420;

// Feature 033 — frontend Sentry DSN, baked at build time, PROD ONLY.
// The prod build (`scripts/_prod-build-env.sh`) exports `VITE_SENTRY_DSN` from
// the git-ignored `.env.prod`; dev/staging leave it unset so the webview SDK
// no-ops (mirrors the Rust baked-DSN gating). `.env.prod` is a non-standard
// name Vite won't auto-load, so forward it explicitly via `define`. Empty
// string when absent → `initFrontendSentry()` treats that as "disabled".
// @ts-expect-error process is a nodejs global
const sentryDsn = process.env.VITE_SENTRY_DSN ?? "";

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [react(), tailwindcss()],

  define: {
    "import.meta.env.VITE_SENTRY_DSN": JSON.stringify(sentryDsn),
  },

  resolve: {
    alias: {
      "@": resolve(__dirname, "src"),
    },
  },

  build: {
    rollupOptions: {
      input: {
        app: resolve(__dirname, "app.html"),
        hud: resolve(__dirname, "hud.html"),
      },
    },
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: devPort,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: devPort + 1,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
