import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';

export default defineConfig({
  root: fileURLToPath(new URL('./frontend', import.meta.url)),
  base: './',
  plugins: [vue(), {
    name: 'plugin-classic-script',
    apply: 'build',
    transformIndexHtml: { order: 'post', handler: (html) => html.replace('type="module" crossorigin', 'defer') },
  }],
  build: {
    outDir: '../dist/plugin', emptyOutDir: true,
    modulePreload: false, cssCodeSplit: false,
    rolldownOptions: { output: { format: 'iife', codeSplitting: false, entryFileNames: 'app.js', assetFileNames: 'app.[ext]' } },
  },
});
