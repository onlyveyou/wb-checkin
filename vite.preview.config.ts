import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { fileURLToPath } from "node:url";

// 仅用于浏览器预览：root 指向 preview/，产物独立输出到 preview-dist/，不影响应用打包。
export default defineConfig({
  root: fileURLToPath(new URL("./preview", import.meta.url)),
  base: "./",
  plugins: [react()],
  build: {
    outDir: fileURLToPath(new URL("./preview-dist", import.meta.url)),
    emptyOutDir: true,
    minify: false,
    sourcemap: false,
  },
});
