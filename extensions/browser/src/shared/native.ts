import { NATIVE_HOST_NAME, type NativeBridgeRequest, type NativeBridgeResponse } from "@slipstream/ipc-types";
import { extension } from "./browser";
import { emptyState, type BridgeState, type Status } from "./state";

type Pending = {
  request: NativeBridgeRequest;
  resolve: (response: NativeBridgeResponse) => void;
};

const disconnected: NativeBridgeResponse = {
  type: "error",
  code: "pipe_unavailable",
  message: "SlipStream desktop is disconnected.",
};

export class NativeClient {
  state: BridgeState = emptyState;
  private port?: chrome.runtime.Port;
  private queue: Pending[] = [];
  private pending?: Pending;
  private responseTimeout?: ReturnType<typeof setTimeout>;
  private pollTimer?: ReturnType<typeof setTimeout>;
  private retryTimer?: ReturnType<typeof setTimeout>;
  private backoff = 500;

  constructor(private readonly changed: (state: BridgeState) => void) {}

  start(): void {
    if (this.port || this.retryTimer !== undefined) return;
    try {
      const port = extension.runtime.connectNative(NATIVE_HOST_NAME);
      this.port = port;
      port.onMessage.addListener((message: NativeBridgeResponse) => {
        if (this.port !== port) return;
        if (!this.pending || !message || typeof message.type !== "string") {
          this.losePort();
          return;
        }
        const pending = this.pending;
        this.pending = undefined;
        clearTimeout(this.responseTimeout);
        pending.resolve(message);
        if (message.type === "error" && message.code === "pipe_unavailable") {
          this.losePort();
        } else {
          this.drain();
        }
      });
      port.onDisconnect.addListener(() => {
        void extension.runtime.lastError;
        if (this.port === port) this.losePort(false);
      });
      void this.poll();
    } catch {
      this.losePort();
    }
  }

  wake(): void {
    if (!this.port) {
      if (this.retryTimer !== undefined) {
        clearTimeout(this.retryTimer);
        this.retryTimer = undefined;
      }
      this.start();
      return;
    }
    const statusQueued = this.pending?.request.type === "get_status"
      || this.queue.some((entry) => entry.request.type === "get_status");
    if (!statusQueued) {
      clearTimeout(this.pollTimer);
      this.pollTimer = undefined;
      void this.poll();
    }
  }

  send(request: NativeBridgeRequest): Promise<NativeBridgeResponse> {
    if (!this.state.connected) return Promise.resolve(disconnected);
    return this.enqueue(request);
  }

  probe(request: Extract<NativeBridgeRequest, { type: "probe_media" }>, signal: AbortSignal): Promise<NativeBridgeResponse> {
    if (!this.state.connected || signal.aborted) return Promise.resolve(disconnected);
    // Native Messaging has no request IDs. Isolate discovery so a slow probe
    // cannot hold the status and user-action connection.
    return new Promise((resolve) => {
      let port: chrome.runtime.Port | undefined;
      const finish = (response: NativeBridgeResponse, disconnect = true) => {
        signal.removeEventListener("abort", abort);
        resolve(response);
        if (disconnect) port?.disconnect();
      };
      const abort = () => finish({ type: "error", code: "request_timeout", message: "Media discovery timed out." });
      try {
        port = extension.runtime.connectNative(NATIVE_HOST_NAME);
        signal.addEventListener("abort", abort, { once: true });
        port.onMessage.addListener((response: NativeBridgeResponse) => finish(response));
        port.onDisconnect.addListener(() => {
          void extension.runtime.lastError;
          finish(disconnected, false);
        });
        port.postMessage(request);
      } catch {
        finish(disconnected);
      }
    });
  }

  private enqueue(request: NativeBridgeRequest): Promise<NativeBridgeResponse> {
    if (!this.port) return Promise.resolve(disconnected);
    if (this.queue.length >= 32) {
      return Promise.resolve({ type: "error", code: "busy", message: "SlipStream is busy. Try again shortly." });
    }
    return new Promise((resolve) => {
      this.queue.push({ request, resolve });
      this.drain();
    });
  }

  private drain(): void {
    if (this.pending || !this.port) return;
    const pending = this.queue.shift();
    if (!pending) return;
    this.pending = pending;
    this.responseTimeout = setTimeout(() => this.losePort(), 45_000);
    try {
      this.port.postMessage(pending.request);
    } catch {
      this.losePort();
    }
  }

  private async poll(): Promise<void> {
    const port = this.port;
    const response = await this.enqueue({ type: "get_status" });
    if (this.port !== port || !port) return;
    if (isStatus(response)) {
      this.backoff = 500;
      this.state = { connected: true, theme: response.theme, accent: response.accent, batch: response.batch };
      this.changed(this.state);
    } else if (!this.state.connected || response.type !== "error" || response.code !== "busy") {
      this.losePort();
      return;
    }
    this.pollTimer = setTimeout(() => void this.poll(), 1500);
  }

  private losePort(disconnect = true): void {
    clearTimeout(this.responseTimeout);
    clearTimeout(this.pollTimer);
    const port = this.port;
    this.port = undefined;
    if (disconnect) port?.disconnect();
    this.pending?.resolve(disconnected);
    this.pending = undefined;
    for (const queued of this.queue.splice(0)) queued.resolve(disconnected);
    if (this.state.connected) {
      this.state = { ...this.state, connected: false };
      this.changed(this.state);
    }
    if (this.retryTimer === undefined) {
      this.retryTimer = setTimeout(() => {
        this.retryTimer = undefined;
        this.start();
      }, this.backoff);
      this.backoff = Math.min(this.backoff * 2, 30_000);
    }
  }
}

function isStatus(response: NativeBridgeResponse): response is Status {
  return response.type === "status"
    && ["dark", "light", "system"].includes(response.theme)
    && ["blue", "mono"].includes(response.accent)
    && Boolean(response.batch)
    && Array.isArray(response.batch.transfers);
}
