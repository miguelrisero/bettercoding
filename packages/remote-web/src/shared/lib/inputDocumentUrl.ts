import type { LocalApiRequestOptions } from "@/shared/lib/localApiTransport";
import { buildRemoteSessionBaseUrl } from "@/shared/lib/relayBackendApi";
import {
  invalidateRemoteSessionId,
  resolveRemoteHostContext,
  tryRefreshRelayHostSigningSession,
} from "./relay/context";
import { resolveRelayHostIdForCurrentPage } from "./relay/routing";
import { appendSignatureToPath, buildRelaySignature } from "./relay/signing";

export async function inputDocumentUrl(
  path: string,
  options: LocalApiRequestOptions = {},
): Promise<string> {
  const host =
    options.relayHostId ?? options.hostId ?? resolveRelayHostIdForCurrentPage();
  if (!host) throw new Error("Input host context is required");
  const normalized = path.replace(/^\/api\/host\/[^/]+\//, "/api/");
  if (
    !/^\/api\/workspaces\/[^/]+\/inputs\/[^/]+\/versions\/\d+\/content$/.test(
      normalized,
    )
  )
    throw new Error("Invalid Input content route");
  invalidateRemoteSessionId(host);
  const baseContext = await resolveRemoteHostContext(host);
  const context =
    (await tryRefreshRelayHostSigningSession(baseContext)) ?? baseContext;
  const signature = await buildRelaySignature(
    context.pairedHost,
    "GET",
    normalized,
    new Uint8Array(),
  );
  return `${buildRemoteSessionBaseUrl(host, context.sessionId)}${appendSignatureToPath(normalized, signature)}`;
}
