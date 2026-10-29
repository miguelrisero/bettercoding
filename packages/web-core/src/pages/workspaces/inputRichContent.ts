import type { InputBridge } from './inputBridge';

export function readInputImageRequest(
  data: unknown,
  bridge: InputBridge
): { index: number; src: string } | null {
  if (!bridge.valid || !data || typeof data !== 'object') return null;
  const value = data as Record<string, unknown>;
  if (
    value.type !== 'bc.input.image.request' ||
    value.protocol !== 1 ||
    value.bridge_id !== bridge.id ||
    value.input_id !== bridge.inputId ||
    value.version !== bridge.version ||
    typeof value.index !== 'number' ||
    !Number.isInteger(value.index) ||
    value.index < 0 ||
    value.index >= 2000 ||
    typeof value.src !== 'string' ||
    value.src.length > 8192
  )
    return null;
  return { index: value.index, src: value.src };
}

// Match the existing Input byte cap; images larger than this stay as alt text.
const MAX_IMAGE_BYTES = 256 * 1024;
const RASTER_TYPE = /^image\/(?:png|jpeg|gif|webp|avif|bmp|x-icon)$/;

export function inputImagePath(
  source: string,
  workspaceId: string,
  hostId: string | null,
  origin: string
): string | null {
  const root = `/api/workspaces/${workspaceId}/files/preview/`;
  let path = source;
  if (/^https?:\/\//.test(path)) {
    let url: URL;
    try {
      url = new URL(path);
    } catch {
      return null;
    }
    if (
      url.origin !== origin ||
      url.search ||
      url.hash ||
      url.username ||
      url.password
    )
      return null;
    path = url.pathname;
  }
  if (hostId && path.startsWith(`/api/host/${hostId}/`))
    path = '/api' + path.slice(`/api/host/${hostId}`.length);
  if (path.startsWith('.bettercoding-uploads/')) path = root + path;
  if (!path.startsWith(root)) return null;
  try {
    const parts = path.slice(root.length).split('/').map(decodeURIComponent);
    if (
      parts.some(
        (part) =>
          !part ||
          part === '.' ||
          part === '..' ||
          /[%\\/?#]/.test(part) ||
          Array.from(part).some((character) => character.charCodeAt(0) < 32)
      )
    )
      return null;
    if (!/\.(?:png|jpe?g|gif|webp|avif|bmp|ico)$/i.test(parts.at(-1)!))
      return null;
    return root + parts.map(encodeURIComponent).join('/');
  } catch {
    return null;
  }
}

export async function readInputImage(
  response: Response
): Promise<string | null> {
  const type = response.headers.get('content-type')?.split(';')[0].trim() ?? '';
  if (
    !response.ok ||
    response.redirected ||
    !RASTER_TYPE.test(type) ||
    !response.body
  )
    return null;
  const reader = response.body.getReader();
  let binary = '';
  try {
    let chunk = await reader.read();
    while (!chunk.done) {
      const value = chunk.value;
      if (binary.length + value.length > MAX_IMAGE_BYTES) return null;
      for (const byte of value) binary += String.fromCharCode(byte);
      chunk = await reader.read();
    }
    return `data:${type};base64,${btoa(binary)}`;
  } finally {
    await reader.cancel();
  }
}
