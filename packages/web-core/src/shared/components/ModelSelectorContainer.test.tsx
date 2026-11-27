import { createElement, type ReactElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it, vi } from 'vitest';
import {
  BaseCodingAgent,
  type ExecutorConfig,
  type ModelSelectorConfig,
} from 'shared/types';
import { ModelSelectorContainer } from './ModelSelectorContainer';

const effort = [
  { id: 'high', label: 'High', is_default: false },
  { id: 'max', label: 'Max', is_default: true },
];
const discovered: ModelSelectorConfig = {
  providers: [],
  models: [
    { id: 'opus', name: 'Opus', reasoning_options: effort },
    { id: 'haiku', name: 'Haiku', reasoning_options: [] },
  ],
  default_model: 'opus',
  agents: [],
  permissions: [],
};

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock('@/shared/hooks/useUserSystem', () => ({
  useUserSystem: () => ({
    profiles: null,
    setProfiles: vi.fn(),
    reloadSystem: vi.fn(),
  }),
}));
vi.mock('@/shared/hooks/useTheme', () => ({
  useTheme: () => ({ theme: 'light' }),
  getResolvedTheme: () => 'light',
}));
vi.mock('@/shared/hooks/useExecutorDiscovery', () => ({
  useModelSelectorConfig: () => ({
    config: discovered,
    loadingModels: false,
    error: null,
  }),
}));
vi.mock('@/shared/lib/api', () => ({ profilesApi: { save: vi.fn() } }));
vi.mock('@bettercoding/ui/components/ModelSelectorPopover', async () => {
  const { DropdownMenu } = await import('@bettercoding/ui/components/Dropdown');
  return {
    ModelSelectorPopover: ({ trigger }: { trigger: ReactElement }) =>
      createElement(DropdownMenu, null, trigger),
  };
});

function modelLabel(
  executorConfig: ExecutorConfig,
  sessionId: string | undefined
): string {
  const html = renderToStaticMarkup(
    createElement(ModelSelectorContainer, {
      agent: BaseCodingAgent.CLAUDE_CODE,
      workspaceId: 'workspace',
      sessionId,
      onAdvancedSettings: vi.fn(),
      presets: ['DEFAULT'],
      selectedPreset: 'DEFAULT',
      onPresetSelect: vi.fn(),
      onOverrideChange: vi.fn(),
      executorConfig,
      presetOptions: null,
    })
  );
  return html.replace(/<[^>]+>/g, ' ');
}

describe('ModelSelectorContainer label', () => {
  it("shows the session's own model and effort", () => {
    const label = modelLabel(
      { executor: BaseCodingAgent.CLAUDE_CODE, model_id: 'haiku' },
      'session'
    );
    expect(label).toContain('Haiku');
    expect(label).not.toContain('Opus');
    expect(label).not.toContain('Max');
  });

  it('shows no model when the session model is unknown', () => {
    const label = modelLabel(
      { executor: BaseCodingAgent.CLAUDE_CODE },
      'session'
    );
    expect(label).not.toContain('Opus');
    expect(label).not.toContain('Max');
    expect(label).toContain('modelSelector.default');
  });

  it('offers the executor default before a session exists', () => {
    const label = modelLabel(
      { executor: BaseCodingAgent.CLAUDE_CODE },
      undefined
    );
    expect(label).toContain('Opus · Max');
  });
});
