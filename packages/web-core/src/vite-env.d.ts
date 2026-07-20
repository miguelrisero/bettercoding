/// <reference types="vite/client" />

interface ImportMetaEnv {
  // TODO(bc-legacy-cleanup): migrate this VITE_VK_ build-time variable with
  // its CI configuration.
  readonly VITE_VK_SHARED_API_BASE?: string;
  readonly VITE_RELAY_API_BASE_URL?: string;
}

declare const __APP_VERSION__: string;
