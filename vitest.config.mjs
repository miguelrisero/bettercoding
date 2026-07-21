import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = path.dirname(fileURLToPath(import.meta.url));

export default {
  resolve: {
    alias: [
      {
        find: /^@\//,
        replacement: `${path.resolve(repoRoot, 'packages/web-core/src')}/`,
      },
      {
        find: 'shared',
        replacement: path.resolve(repoRoot, 'shared'),
      },
    ],
  },
};
