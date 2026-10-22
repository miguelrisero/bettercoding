import { describe, expect, it } from 'vitest';

import { workspaceFilesApi } from './api';

describe('workspaceFilesApi.previewUrl', () => {
  it.each([
    ['report.html', 'report.html'],
    ['nested dir/report #1?.html', 'nested%20dir/report%20%231%3F.html'],
    ['資料/écran.png', '%E8%B3%87%E6%96%99/%C3%A9cran.png'],
    ['100%/a+b.txt', '100%25/a%2Bb.txt'],
  ])(
    'encodes file path %s while preserving directory separators',
    (path, encoded) => {
      expect(workspaceFilesApi.previewUrl('workspace-id', path)).toBe(
        `/api/workspaces/workspace-id/files/preview/${encoded}`
      );
    }
  );
});
