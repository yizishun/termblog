import { defineConfig } from "vite";

// 开发: vite dev server (5173) 把 /ws 代理给 axum (8080);
// 生产: vite build 产物直接由 axum 的 ServeDir("frontend/dist") 服务。
export default defineConfig({
  server: {
    proxy: {
      "/ws": { target: "http://127.0.0.1:8080", ws: true },
    },
  },
});
