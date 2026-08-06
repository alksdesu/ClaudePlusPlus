import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: {
    port: 1430,
    strictPort: true,
  },
  build: {
    target: "chrome110",
    outDir: "dist",
  },
});
