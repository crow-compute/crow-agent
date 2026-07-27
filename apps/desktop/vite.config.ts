import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  envPrefix: ["VITE_", "TAURI_"],
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test-setup.ts"],
  },
  build: {
    target: ["es2022", "chrome105", "safari13"],
    minify: "esbuild",
    sourcemap: false,
  },
});
