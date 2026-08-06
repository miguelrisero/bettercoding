import { useContext } from 'react';
import { createHmrContext } from '@/shared/lib/hmrContext';
import type {
  Config,
  Environment,
  BaseAgentCapability,
  LoginStatus,
} from 'shared/types';
import type { ExecutorProfile } from 'shared/types';

export interface UserSystemState {
  appVersion: string | null;
  previewProxyPort: number | null;
  config: Config | null;
  environment: Environment | null;
  profiles: Record<string, ExecutorProfile> | null;
  capabilities: Record<string, BaseAgentCapability[]> | null;
  machineId: string | null;
  loginStatus: LoginStatus | null;
  remoteAuthDegraded: string | null;
  /**
   * Whether the server has the CLI↔UI handover switched on. The feature ships
   * dark; while it is off the per-session native-feed WebSocket must never be
   * opened. Defaults to `false` until the config query resolves, so a slow
   * config load can never transiently open the stream.
   */
  cliHandoverEnabled: boolean;
}

export interface UserSystemContextType {
  // Full system state
  system: UserSystemState;

  // Hot path - config helpers (most frequently used)
  appVersion: string | null;
  previewProxyPort: number | null;
  config: Config | null;
  updateConfig: (updates: Partial<Config>) => void;
  updateAndSaveConfig: (updates: Partial<Config>) => Promise<boolean>;
  saveConfig: () => Promise<boolean>;

  // System data access
  environment: Environment | null;
  profiles: Record<string, ExecutorProfile> | null;
  capabilities: Record<string, BaseAgentCapability[]> | null;
  machineId: string | null;
  loginStatus: LoginStatus | null;
  remoteAuthDegraded: string | null;
  cliHandoverEnabled: boolean;
  setEnvironment: (env: Environment | null) => void;
  setProfiles: (profiles: Record<string, ExecutorProfile> | null) => void;
  setCapabilities: (caps: Record<string, BaseAgentCapability[]> | null) => void;

  // Reload system data
  reloadSystem: () => Promise<void>;

  // State
  loading: boolean;
}

export const UserSystemContext = createHmrContext<
  UserSystemContextType | undefined
>('UserSystemContext', undefined);

export function useUserSystem() {
  const context = useContext(UserSystemContext);
  if (context === undefined) {
    throw new Error('useUserSystem must be used within a UserSystemProvider');
  }
  return context;
}
