import { defineConfig } from "vite";

export default defineConfig({
  base: process.env.RXLS_BASE_PATH || "/",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: process.env.RXLS_SOURCEMAP !== "0",
    target: "es2022"
  },
  server: {
    strictPort: true
  },
  preview: {
    strictPort: true
  }
});
