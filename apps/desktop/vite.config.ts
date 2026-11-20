import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { defineConfig } from "vitest/config";

const ipcTypes = decodeURIComponent(new URL("../../packages/ipc-types/src/index.ts", import.meta.url).pathname)
  .replace(/^\/(?=[A-Za-z]:\/)/, "");

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: { port: 1420, strictPort: true },
  resolve: { alias: { "@slipstream/ipc-types": ipcTypes } },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    globals: true,
    pool: "vmThreads",
    maxWorkers: 1,
  },
});
