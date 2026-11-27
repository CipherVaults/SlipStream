import { cp, copyFile, rm } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { build } from "vite";

const path = (relative) => fileURLToPath(new URL(relative, import.meta.url));
const root = path(".");
const outDir = path("dist/chrome");
const firefoxDir = path("dist/firefox");
const options = { root, configFile: path("vite.config.ts"), base: "./" };

await build({
  ...options,
  build: { outDir, rollupOptions: { input: path("popup.html") } },
});
for (const entry of ["early", "background", "content"]) {
  await build({
    ...options,
    build: {
      outDir,
      emptyOutDir: false,
      lib: { entry: path("src/" + entry + ".ts"), name: "SlipStream", formats: ["iife"], fileName: () => entry + ".js" },
    },
  });
}
await cp(path("icons"), path("dist/chrome/icons"), { recursive: true });
await rm(firefoxDir, { recursive: true, force: true });
await cp(outDir, firefoxDir, { recursive: true });
await copyFile(path("manifest.firefox.json"), path("dist/firefox/manifest.json"));
await copyFile(path("manifest.json"), path("dist/chrome/manifest.json"));
