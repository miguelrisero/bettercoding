import { describe, it, expect } from 'vitest';
import type { ModelSelectorConfig, ReasoningOption } from 'shared/types';

import { appendPresetModel } from './modelSelector';

const effortOptions: ReasoningOption[] = [
  { id: 'low', label: 'Low', is_default: false },
  { id: 'high', label: 'High', is_default: true },
  { id: 'ultracode', label: 'Ultracode', is_default: false },
];

function claudeConfig(): ModelSelectorConfig {
  return {
    providers: [],
    models: [
      {
        id: 'opus',
        name: 'Opus',
        provider_id: null,
        reasoning_options: effortOptions,
      },
      {
        id: 'haiku',
        name: 'Haiku',
        provider_id: null,
        reasoning_options: [],
      },
    ],
    default_model: 'opus',
    agents: [],
    permissions: [],
  };
}

function providerConfig(): ModelSelectorConfig {
  return {
    providers: [{ id: 'openai', name: 'OpenAI' }],
    models: [
      {
        id: 'gpt-5',
        name: 'GPT-5',
        provider_id: 'openai',
        reasoning_options: [{ id: 'high', label: 'High', is_default: true }],
      },
    ],
    default_model: 'openai/gpt-5',
    agents: [],
    permissions: [],
  };
}

describe('appendPresetModel', () => {
  it('injects an effort-capable Claude preset with inherited reasoning options', () => {
    const result = appendPresetModel(claudeConfig(), 'claude-fable-5', true);
    expect(result).not.toBeNull();
    const injected = result!.models[0];
    expect(injected.id).toBe('claude-fable-5');
    expect(injected.provider_id).toBeNull();
    // Fable is effort-capable, so it inherits the effort options from the first
    // config model that has them (opus) — including ultracode.
    expect(injected.reasoning_options.map((o) => o.id)).toEqual([
      'low',
      'high',
      'ultracode',
    ]);
  });

  it('injects a free-text custom effort-capable id with inherited options', () => {
    // Custom (typed) ids get the same effort fallback as the profile preset.
    const result = appendPresetModel(claudeConfig(), 'claude-opus-4-8', true);
    const injected = result!.models[0];
    expect(injected.id).toBe('claude-opus-4-8');
    expect(injected.reasoning_options.map((o) => o.id)).toEqual([
      'low',
      'high',
      'ultracode',
    ]);
  });

  it('does not inherit effort options when the fallback is disabled', () => {
    // Non-Claude executors pass enableEffortFallback=false (the default): even a
    // provider-less id containing "opus" must not invent effort options.
    const result = appendPresetModel(claudeConfig(), 'claude-fable-5');
    expect(result!.models[0].id).toBe('claude-fable-5');
    expect(result!.models[0].reasoning_options).toEqual([]);
  });

  it('does not invent effort options for a non-effort-capable preset', () => {
    const result = appendPresetModel(claudeConfig(), 'some-random-model', true);
    expect(result!.models[0].id).toBe('some-random-model');
    expect(result!.models[0].reasoning_options).toEqual([]);
  });

  it('leaves provider-scoped presets without inherited effort options', () => {
    // codex-style providers path: the injected id belongs to a provider, so no
    // reasoning fallback is applied even if the id string matched the heuristic.
    const result = appendPresetModel(
      providerConfig(),
      'openai/opus-custom',
      true
    );
    const injected = result!.models[0];
    expect(injected.id).toBe('opus-custom');
    expect(injected.provider_id).toBe('openai');
    expect(injected.reasoning_options).toEqual([]);
  });

  it('returns the config unchanged when the preset already exists', () => {
    const config = claudeConfig();
    const result = appendPresetModel(config, 'opus', true);
    expect(result).toBe(config);
  });

  it('returns the config unchanged when no preset is given', () => {
    const config = claudeConfig();
    expect(appendPresetModel(config, null, true)).toBe(config);
    expect(appendPresetModel(config, undefined, true)).toBe(config);
  });
});
