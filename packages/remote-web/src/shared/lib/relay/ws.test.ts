import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { createRelaySignedWebSocket } from "./ws";
import type { RelaySignedWsEnvelope, RelayWsSigningContext } from "./types";

const encoder = new TextEncoder();
const challenge = new Uint8Array([0, 255, 1, 128, 42]);

function base64(bytes: Uint8Array): string {
  return btoa(String.fromCharCode(...bytes));
}

async function signingInput(
  session: string,
  nonce: string,
  sequence: number,
  type: string,
  payload: Uint8Array,
): Promise<Uint8Array<ArrayBuffer>> {
  const digest = await crypto.subtle.digest("SHA-256", new Uint8Array(payload));
  return encoder.encode(
    [
      "v1",
      session,
      nonce,
      String(sequence),
      type,
      base64(new Uint8Array(digest)),
    ].join("|"),
  );
}

class RawSocket extends EventTarget {
  readyState = 1;
  binaryType: BinaryType = "blob";
  sent: RelaySignedWsEnvelope[] = [];
  closed: Array<[number | undefined, string | undefined]> = [];

  send(value: string): void {
    this.sent.push(JSON.parse(value) as RelaySignedWsEnvelope);
  }

  close(code?: number, reason?: string): void {
    this.readyState = 3;
    this.closed.push([code, reason]);
  }

  receive(value: RelaySignedWsEnvelope): void {
    this.dispatchEvent(
      new MessageEvent("message", { data: JSON.stringify(value) }),
    );
  }
}

async function fixture() {
  const server = (await crypto.subtle.generateKey("Ed25519", false, [
    "sign",
    "verify",
  ])) as CryptoKeyPair;
  const client = (await crypto.subtle.generateKey("Ed25519", false, [
    "sign",
    "verify",
  ])) as CryptoKeyPair;
  const context: RelayWsSigningContext = {
    signingSessionId: "terminal-session",
    requestNonce: "terminal-request",
    inboundSeq: 0,
    outboundSeq: 0,
    signingKey: client.privateKey,
    serverVerifyKey: server.publicKey,
  };
  const raw = new RawSocket();
  const socket = createRelaySignedWebSocket(
    raw as unknown as WebSocket,
    context,
  );
  const messages: unknown[] = [];
  socket.onmessage = (event) => messages.push(event.data);
  async function incoming(
    type: "ping" | "pong",
    sequence = 1,
    session = context.signingSessionId,
    nonce = context.requestNonce,
  ): Promise<RelaySignedWsEnvelope> {
    const signature = await crypto.subtle.sign(
      "Ed25519",
      server.privateKey,
      await signingInput(session, nonce, sequence, type, challenge),
    );
    return {
      version: 1,
      seq: sequence,
      msg_type: type,
      payload_b64: base64(challenge),
      signature_b64: base64(new Uint8Array(signature)),
    };
  }
  async function verify(frame: RelaySignedWsEnvelope): Promise<boolean> {
    const bytes = (value: string) =>
      Uint8Array.from(atob(value), (char) => char.charCodeAt(0));
    return crypto.subtle.verify(
      "Ed25519",
      client.publicKey,
      bytes(frame.signature_b64),
      await signingInput(
        context.signingSessionId,
        context.requestNonce,
        frame.seq,
        frame.msg_type,
        bytes(frame.payload_b64),
      ),
    );
  }
  return { raw, socket, context, messages, incoming, verify };
}

beforeEach(() => {
  vi.stubGlobal("WebSocket", { OPEN: 1, CONNECTING: 0 });
});
afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("authenticated relay heartbeat", () => {
  it("queues an exact-payload Pong between already queued and later application sends", async () => {
    const f = await fixture();
    const ping = await f.incoming("ping");
    let release!: () => void;
    const blocked = new Promise<void>((resolve) => {
      release = resolve;
    });
    const sign = crypto.subtle.sign.bind(crypto.subtle);
    const firstSign = vi
      .spyOn(crypto.subtle, "sign")
      .mockImplementationOnce(async (...args) => {
        await blocked;
        return sign(...args);
      });
    f.socket.send("before");
    await vi.waitFor(() => expect(firstSign).toHaveBeenCalledTimes(1));
    f.raw.receive(ping);
    await vi.waitFor(() => expect(f.context.inboundSeq).toBe(1));
    f.socket.send("after");
    expect(f.raw.sent).toEqual([]);
    release();
    await vi.waitFor(() => expect(f.raw.sent).toHaveLength(3));
    expect(f.raw.sent.map((frame) => [frame.seq, frame.msg_type])).toEqual([
      [1, "text"],
      [2, "pong"],
      [3, "text"],
    ]);
    expect(f.raw.sent.map((frame) => frame.payload_b64)).toEqual([
      base64(encoder.encode("before")),
      base64(challenge),
      base64(encoder.encode("after")),
    ]);
    for (const frame of f.raw.sent) expect(await f.verify(frame)).toBe(true);
    expect(f.context.outboundSeq).toBe(3);
    expect(f.messages).toEqual([]);
    expect(f.raw.closed).toEqual([]);
  });

  it("keeps a verified decoded Pong internal and sends zero reply envelopes", async () => {
    const f = await fixture();
    f.raw.receive(await f.incoming("pong"));
    await vi.waitFor(() => expect(f.context.inboundSeq).toBe(1));
    f.socket.send("barrier");
    await vi.waitFor(() => expect(f.raw.sent).toHaveLength(1));
    expect(f.raw.sent[0].msg_type).toBe("text");
    expect(f.messages).toEqual([]);
  });

  it.each(["signature", "session", "nonce", "sequence"] as const)(
    "refuses a Ping with an invalid %s before producing a Pong",
    async (invalid) => {
      const f = await fixture();
      vi.spyOn(console, "error").mockImplementation(() => {});
      const frame = await f.incoming(
        "ping",
        invalid === "sequence" ? 2 : 1,
        invalid === "session" ? "foreign-session" : f.context.signingSessionId,
        invalid === "nonce" ? "foreign-request" : f.context.requestNonce,
      );
      if (invalid === "signature")
        frame.signature_b64 = base64(new Uint8Array(64));
      f.raw.receive(frame);
      await vi.waitFor(() =>
        expect(f.raw.closed).toEqual([[1002, "Invalid relay frame"]]),
      );
      expect(f.context.inboundSeq).toBe(0);
      expect(f.context.outboundSeq).toBe(0);
      expect(f.raw.sent).toEqual([]);
      expect(f.messages).toEqual([]);
    },
  );
});
