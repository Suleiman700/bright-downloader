import { defineConfig } from "vite";

// Tauri expects a fixed dev port and a build target matching WebView2.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    // Rust build output churns constantly and holds locked DLLs on Windows.
    watch: { ignored: ["**/target/**", "**/src-tauri/**", "**/crates/**", "**/extension/**"] },
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: { target: "chrome105", outDir: "dist", emptyOutDir: true },
});
