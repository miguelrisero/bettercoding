/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_BC_SHARED_API_BASE?: string;
  readonly VITE_RELAY_API_BASE_URL?: string;
}

declare const __APP_VERSION__: string;
