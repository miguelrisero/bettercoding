import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  invalidate: vi.fn(),
  resolve: vi.fn(),
  refresh: vi.fn(),
  sign: vi.fn(),
  append: vi.fn(),
  base: vi.fn(),
}));
vi.mock("./relay/context", () => ({
  invalidateRemoteSessionId: mocks.invalidate,
  resolveRemoteHostContext: mocks.resolve,
  tryRefreshRelayHostSigningSession: mocks.refresh,
}));
vi.mock("./relay/routing", () => ({
  resolveRelayHostIdForCurrentPage: () => null,
}));
vi.mock("./relay/signing", () => ({
  buildRelaySignature: mocks.sign,
  appendSignatureToPath: mocks.append,
}));
vi.mock("@/shared/lib/relayBackendApi", () => ({
  buildRemoteSessionBaseUrl: mocks.base,
}));
import { inputDocumentUrl } from "./inputDocumentUrl";

const path = "/api/workspaces/w/inputs/i/versions/1/content";
beforeEach(() => {
  vi.clearAllMocks();
  mocks.resolve.mockResolvedValue({
    pairedHost: { signing_session_id: "expired" },
    sessionId: "browser-session",
  });
  mocks.refresh.mockResolvedValue({
    pairedHost: { signing_session_id: "fresh" },
    sessionId: "browser-session",
  });
  mocks.sign.mockResolvedValue({ nonce: "new-nonce" });
  mocks.append.mockImplementation(
    (route: string) => `${route}?signature=fresh`,
  );
  mocks.base.mockReturnValue("https://relay.test/session");
});

describe("Input relay document authorization", () => {
  it("refreshes the browser and signing sessions before signing the document", async () => {
    const url = await inputDocumentUrl(path, { hostId: "host" });
    expect(mocks.invalidate).toHaveBeenCalledWith("host");
    expect(mocks.sign).toHaveBeenCalledWith(
      { signing_session_id: "fresh" },
      "GET",
      path,
      expect.any(Uint8Array),
    );
    expect(url).toBe(`https://relay.test/session${path}?signature=fresh`);
    expect(mocks.refresh.mock.invocationCallOrder[0]).toBeLessThan(
      mocks.sign.mock.invocationCallOrder[0],
    );
  });
  it("creates another signed URL when the trusted host retries", async () => {
    await inputDocumentUrl(path, { hostId: "host" });
    await inputDocumentUrl(path, { hostId: "host" });
    expect(mocks.invalidate).toHaveBeenCalledTimes(2);
    expect(mocks.refresh).toHaveBeenCalledTimes(2);
    expect(mocks.sign).toHaveBeenCalledTimes(2);
  });
  it("uses the existing session when proactive refresh is unavailable", async () => {
    mocks.refresh.mockResolvedValue(null);
    await inputDocumentUrl(path, { hostId: "host" });
    expect(mocks.sign).toHaveBeenCalledWith(
      { signing_session_id: "expired" },
      "GET",
      path,
      expect.any(Uint8Array),
    );
  });
});
