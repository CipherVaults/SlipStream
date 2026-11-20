import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

export default defineConfig({
  resolve: {
    alias: {
      "@slipstream/ipc-types": fileURLToPath(new URL("../../packages/ipc-types/src/index.ts", import.meta.url)),
    },
  },
});
