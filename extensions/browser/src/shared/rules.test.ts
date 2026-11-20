import type { MediaProbeResult } from "@slipstream/ipc-types";
import { afterEach, describe, expect, it, vi } from "vitest";
import { canAttributeTab, defaults, sameDownloadOrigin, shouldIntercept } from "./downloads";
import { ObservedNetwork } from "./network";
import { isRefreshableHttpTransfer } from "./state";
import { isMediaStream, mediaMime, scopedHeaders, selectPillFormatId, StreamNavigationState } from "./streams";

afterEach(() => {
  vi.useRealTimers();
});

describe("media streams", () => {
  it.each(["file.MP4?token=1", "file.webm", "playlist.m3u8#anchor", "manifest.mpd"])("detects %s by HTTP path", (path) => {
    expect(isMediaStream("https://media.example/" + path)).toBe(true);
  });
  it.each(["video/mp4", "audio/flac", "application/vnd.apple.mpegurl", "application/dash+xml"])("detects %s by MIME", (mime) => {
    expect(isMediaStream("http://media.example/download?id=1", mime + "; charset=utf-8")).toBe(true);
  });
  it("ignores unrelated paths, non-HTTP URLs and invalid schemes", () => {
    expect(isMediaStream("https://media.example/api?next=file.mp4", "text/html")).toBe(false);
    expect(isMediaStream("file:///video.mp4", "video/mp4")).toBe(false);
    expect(isMediaStream("blob:https://media.example/id", "video/mp4")).toBe(false);
    expect(isMediaStream("javascript:alert(1)", "video/mp4")).toBe(false);
    expect(mediaMime([{ name: "Content-Type", value: "Video/MP4; charset=UTF-8" }])).toBe("video/mp4");
  });
});

describe("header handoff", () => {
  const page = "https://media.example/player";
  const stream = "https://media.example/stream.m3u8";
  const headers = [
    { name: "cOoKiE", value: "session=test" },
    { name: "referer", value: page },
    { name: "user-agent", value: "Example Agent" },
    { name: "Authorization", value: "Bearer test" },
    { name: "Other", value: "test" },
  ];
  const expected = { Cookie: "session=test", Referer: page, "User-Agent": "Example Agent" };

  it("returns only allowed same-origin headers and checks referrer origin independently", () => {
    expect(scopedHeaders(stream, page, "https://media.example", headers)).toEqual(expected);
    expect(scopedHeaders(stream, page, page, [{ name: "Referer", value: "https://other.example/private" }])).toEqual({});
    expect(scopedHeaders(stream, page, page, [{ name: "Cookie", value: "a=b\r\nInjected: yes" }])).toEqual({});
  });

  it.each(["http://media.example/stream", "https://media.example:8443/stream", "https://cdn.media.example/stream"])(
    "strips every captured header for %s", (url) => {
      expect(scopedHeaders(url, page, page, headers)).toEqual({});
      expect(scopedHeaders(stream, page, url, headers)).toEqual({});
    },
  );

  it("isolates tabs, URL matches, redirects and navigation", () => {
    const network = new ObservedNetwork();
    const request = { tabId: 8, requestId: "1", url: stream, initiator: page };
    network.recordHeaders(request, headers);
    expect(network.headers(8, stream, page)).toEqual(expected);
    expect(network.headers(7, stream, page)).toEqual({});
    expect(network.headers(8, stream + "?other", page)).toEqual({});
    expect(network.headers(8, stream, "https://another.example")).toEqual({});
    const redirect = "https://other.example/stream.mp4";
    network.recordHeaders({ ...request, url: redirect }, headers);
    expect(network.headers(8, redirect, page)).toEqual({});
    expect(network.recordStream({ ...request, url: redirect }, page, [])).toBe(true);
    expect(network.stream(8, page)?.url).toBe(redirect);
    network.clear(8);
    expect(network.stream(8, page)).toBeUndefined();
    expect(network.headers(8, stream, page)).toEqual({});
  });
});

describe("download interception", () => {
  const item = { url: "https://cdn.example/file.zip", filename: "C:\\Downloads\\file.ZIP", fileSize: 1000, referrer: "https://source.example/page" };
  const settings = { ...defaults, minBytes: 1000, extensions: [".zip"] };
  const context = { connected: true, altHeld: false, tabUrl: item.referrer };

  it("accepts an allowed extension at the configured threshold", () => {
    expect(shouldIntercept(item, settings, context)).toBe(true);
    expect(shouldIntercept({ ...item, filename: "" }, settings, context)).toBe(true);
  });

  it("leaves downloads in the browser when Alt is held, the daemon is offline or interception is off", () => {
    expect(shouldIntercept(item, settings, { ...context, altHeld: true })).toBe(false);
    expect(shouldIntercept(item, settings, { ...context, connected: false })).toBe(false);
    expect(shouldIntercept(item, { ...settings, enabled: false }, context)).toBe(false);
  });

  it("leaves disallowed extensions, undersized, unknown-size and non-HTTP downloads in the browser", () => {
    expect(shouldIntercept(item, { ...settings, extensions: ["exe"] }, context)).toBe(false);
    expect(shouldIntercept({ ...item, fileSize: 999 }, settings, context)).toBe(false);
    expect(shouldIntercept({ ...item, fileSize: -1 }, settings, context)).toBe(false);
    expect(shouldIntercept({ ...item, finalUrl: "file:///file.zip" }, settings, context)).toBe(false);
  });

  it("matches exact sites and their subdomains for excludes and mutes", () => {
    expect(shouldIntercept(item, { ...settings, excludedSites: ["source.example"] }, context)).toBe(false);
    expect(shouldIntercept(item, { ...settings, mutedSites: ["cdn.example"] }, context)).toBe(false);
    expect(shouldIntercept(item, { ...settings, excludedSites: ["dn.example"] }, context)).toBe(true);
  });

  it("rejects incognito, container, and cross-origin redirect header attribution", () => {
    expect(canAttributeTab({ referrer: "https://source.example/p", incognito: true }, { url: "https://source.example/p", incognito: false })).toBe(false);
    expect(canAttributeTab({ referrer: "https://source.example/p", cookieStoreId: "firefox-container-1" }, { url: "https://source.example/p", cookieStoreId: "firefox-default" })).toBe(false);
    expect(canAttributeTab({ referrer: "https://source.example/p", incognito: true, cookieStoreId: "firefox-container-1" }, { url: "https://source.example/p", incognito: true, cookieStoreId: "firefox-container-1" })).toBe(true);
    expect(sameDownloadOrigin({ url: "https://source.example/start", finalUrl: "https://cdn.example/file.zip" })).toBe(false);
    expect(sameDownloadOrigin({ url: "https://source.example/start", finalUrl: "https://source.example/file.zip" })).toBe(true);
  });
});

describe("adaptive pill format selection and TTL cache eviction", () => {
  const baseProbe: MediaProbeResult = {
    title: "Demo",
    source_url: "https://media.example/watch?v=1",
    extractor: "generic",
    duration_seconds: 120,
    thumbnail_url: null,
    is_playlist: false,
    playlist_count: null,
    formats: [
      { format_id: "313", label: "2160p VP9", container: "webm", video_codec: "vp9", audio_codec: null, height: 2160, fps: 60, filesize_bytes: 5000, is_audio_only: false, has_video: true, has_audio: false },
      { format_id: "137", label: "1080p H264", container: "mp4", video_codec: "avc1", audio_codec: null, height: 1080, fps: 30, filesize_bytes: 2000, is_audio_only: false, has_video: true, has_audio: false },
      { format_id: "22", label: "1080p muxed", container: "mp4", video_codec: "avc1", audio_codec: "mp4a", height: 1080, fps: 30, filesize_bytes: 2200, is_audio_only: false, has_video: true, has_audio: true },
      { format_id: "251", label: "Opus audio", container: "webm", video_codec: null, audio_codec: "opus", height: null, fps: null, filesize_bytes: 300, is_audio_only: true, has_video: false, has_audio: true },
    ],
    subtitles: [],
  };

  it("pairs adaptive video-only 4K/1080p streams with +bestaudio when audio exists", () => {
    expect(selectPillFormatId(baseProbe, 2160)).toBe("313+bestaudio");
    expect(selectPillFormatId(baseProbe, 1080)).toBe("22");
    const videoOnly1080: MediaProbeResult = {
      ...baseProbe,
      formats: baseProbe.formats.filter((f) => f.format_id !== "22"),
    };
    expect(selectPillFormatId(videoOnly1080, 1080)).toBe("137+bestaudio");
    const silentProbe: MediaProbeResult = {
      ...baseProbe,
      formats: baseProbe.formats.filter((f) => !f.has_audio),
    };
    expect(selectPillFormatId(silentProbe, 2160)).toBe("313");
    expect(selectPillFormatId({ ...baseProbe, formats: [] }, 2160)).toBeUndefined();
  });

  it("purges expired Cookie headers and streams after TTL", () => {
    vi.useFakeTimers();
    const network = new ObservedNetwork();
    const page = "https://media.example/player";
    const stream = "https://media.example/stream.m3u8";
    network.recordHeaders({ tabId: 4, requestId: "req-1", url: stream, initiator: page }, [{ name: "Cookie", value: "sid=secret" }]);
    network.recordStream({ tabId: 4, requestId: "req-1", url: stream, initiator: page }, page, []);
    expect(network.cacheSize).toEqual({ requests: 1, streams: 1 });
    expect(network.headers(4, stream, page)).toEqual({ Cookie: "sid=secret" });

    vi.advanceTimersByTime(120_000);
    expect(network.headers(4, stream, page)).toEqual({});
    expect(network.stream(4, page)).toBeUndefined();
    expect(network.cacheSize).toEqual({ requests: 0, streams: 0 });
  });

  it("only allows refreshing non-completed, non-canceled HTTP transfers", () => {
    expect(isRefreshableHttpTransfer({ kind: "http", status: "failed" })).toBe(true);
    expect(isRefreshableHttpTransfer({ kind: "http", status: "paused" })).toBe(true);
    expect(isRefreshableHttpTransfer({ kind: "http", status: "completed" })).toBe(false);
    expect(isRefreshableHttpTransfer({ kind: "http", status: "canceled" })).toBe(false);
    expect(isRefreshableHttpTransfer({ kind: "media", status: "failed" })).toBe(false);
  });

  it("clears stale streamUrl across SPA navigations and ignores late replies", () => {
    const nav = new StreamNavigationState("https://media.example/watch?v=1");
    nav.update("https://media.example/watch?v=1", "https://media.example/watch?v=1", "https://cdn.example/v1.m3u8");
    expect(nav.resolveTarget("blob:https://media.example/abc", "https://media.example/watch?v=1")).toBe("https://cdn.example/v1.m3u8");

    expect(nav.resolveTarget("blob:https://media.example/def", "https://media.example/watch?v=2")).toBe("https://media.example/watch?v=2");
    nav.update("https://media.example/watch?v=1", "https://media.example/watch?v=2", "https://cdn.example/v1.m3u8");
    expect(nav.streamUrl).toBeUndefined();
  });
});
