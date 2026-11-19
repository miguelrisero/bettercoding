import { resolve } from 'node:path';
import { build } from 'vite';

// Compile the production components and PostCSS without app-shell plugins.
await build({
  configFile: false,
  envDir: '/scratch/home',
  publicDir: false,
  cacheDir: '/scratch/cache/vite',
  esbuild: { jsx: 'automatic' },
  resolve: {
    alias: [
      { find: /^@\//, replacement: `${resolve('../web-core/src')}/` },
      { find: 'shared', replacement: resolve('../../shared') },
    ],
    dedupe: ['react', 'react-dom', '@tanstack/react-query'],
  },
  build: {
    outDir: '/scratch/queue-dist',
    rollupOptions: {
      input: resolve('test-fixtures/input-queue/index.html'),
    },
  },
});
