import { defineConfig } from "vite";

export default defineConfig({
  base: process.env.RXLS_BASE_PATH || "/",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: true,
    target: "es2022"
  },
  server: {
    strictPort: true
  },
  preview: {
    strictPort: true
  }
});
