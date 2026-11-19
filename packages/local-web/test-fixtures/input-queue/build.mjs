import { realpathSync } from 'node:fs';
import { resolve } from 'node:path';
import { build } from 'vite';

const mermaidBundle = realpathSync(
  '../web-core/node_modules/mermaid/dist/mermaid.min.js'
);

// Compile the production components and PostCSS without app-shell plugins.
await build({
  configFile: false,
  envDir: '/scratch/home',
  publicDir: false,
  cacheDir: '/scratch/cache/vite',
  esbuild: { jsx: 'automatic' },
  resolve: {
    alias: [
      {
        find: /^mermaid\/dist\/mermaid\.min\.js\?raw$/,
        replacement: `${mermaidBundle}?raw`,
      },
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
