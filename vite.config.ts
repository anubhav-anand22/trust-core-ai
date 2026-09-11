import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [react()],

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. Ignore Rust dirs. `target/` is at the WORKSPACE ROOT now (not under
      //    src-tauri), and watching it makes Vite's FSWatcher crash on locked
      //    build artifacts (EBUSY on tauri_app_lib.dll) mid-build.
      ignored: ["**/src-tauri/**", "**/target/**", "**/crates/**/target/**"],
    },
  },
}));
