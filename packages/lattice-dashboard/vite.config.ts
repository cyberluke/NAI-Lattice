import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';

/**
 * Embedded preview bundle for `GET /lattice/`.
 * No external origins, no source maps, self-hosted assets only.
 */
export default defineConfig({
  base: '/lattice/',
  plugins: [vue()],
  build: {
    assetsDir: 'assets',
    sourcemap: false,
    target: 'es2022',
    cssCodeSplit: false,
    modulePreload: { polyfill: false },
    rollupOptions: {
      output: {
        entryFileNames: 'assets/[name]-[hash].js',
        chunkFileNames: 'assets/[name]-[hash].js',
        assetFileNames: 'assets/[name]-[hash][extname]',
      },
    },
  },
  server: {
    // Development only: the host control plane is loopback. Production serves
    // the built bundle from the host origin, so this proxy is never bundled.
    proxy: {
      '^/lattice/(api|events)': {
        target: 'http://127.0.0.1:8787',
        changeOrigin: false,
      },
    },
  },
});
