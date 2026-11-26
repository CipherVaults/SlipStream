import type { MediaProbeResult } from "@slipstream/ipc-types";
import { afterEach, describe, expect, it, vi } from "vitest";
import { canAttributeTab, defaults, sameDownloadOrigin, shouldIntercept } from "./downloads";
import { isPublicHttpUrl, ObservedNetwork } from "./network";
import { isRefreshableHttpTransfer } from "./state";
import { isMediaStream, mediaMime, parseContentRangeTotal, parseHlsMaster, probeFormats, scopedHeaders, selectPillFormatId, StreamNavigationState } from "./streams";

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

  it("filters segment paths, transport streams, range parameters and payloads below 300 KB", () => {
    for (const path of ["seg.ts", "seg.M4S", "chunk.m4f", "hls/seg-12.mp4", "file.mp4?range=0-500", "file.mp4?ByteStart=0", "file.mp4?byteend=500"]) {
      expect(isMediaStream("https://media.example/" + path, "video/mp4", 500_000)).toBe(false);
    }
    expect(isMediaStream("https://media.example/chunk", "video/MP2T", 500_000)).toBe(false);
    expect(isMediaStream("https://media.example/file.mp4", "", 299_999)).toBe(false);
    expect(isMediaStream("https://media.example/file.mp4", "", 300_000)).toBe(true);
    expect(isMediaStream("https://media.example/movie.mkv", "", 500_000)).toBe(true);
    expect(isMediaStream("https://media.example/clip.mov", "", 500_000)).toBe(true);
    expect(isMediaStream("https://media.example/master.m3u8", "", 100)).toBe(true);
    expect(isMediaStream("https://media.example/manifest", "application/dash+xml", 100)).toBe(true);
    expect(parseContentRangeTotal([{ name: "Content-Range", value: "bytes 0-65535/1500000000" }])).toBe(1_500_000_000);
    expect(parseContentRangeTotal([{ name: "Content-Range", value: "bytes 0-65535/*" }])).toBeUndefined();
    expect(parseContentRangeTotal([{ name: "Content-Range", value: "invalid" }])).toBeUndefined();
  });

  it("parses, resolves, sorts and deduplicates HLS variants by quality", () => {
    const text = [
      "#EXTM3U",
      "#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720",
      "720/index.m3u8",
      '#EXT-X-STREAM-INF:CODECS="avc1.640028,mp4a.40.2",BANDWIDTH=8000000,RESOLUTION=1920x1080',
      "../1080.m3u8",
      "#EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080",
      "/lower-1080.m3u8",
      "#EXT-X-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=3840x2160",
      "javascript:alert(1)",
      "#EXT-X-STREAM-INF:BANDWIDTH=9500000,RESOLUTION=3840x2160",
      "http://127.0.0.1/internal.m3u8",
      "#EXT-X-STREAM-INF:BANDWIDTH=9600000,RESOLUTION=3840x2160",
      "https://[2002:7f00:1::1]/6to4.m3u8",
      "#EXT-X-STREAM-INF:RESOLUTION=3840x2160",
      "no-bandwidth.m3u8",
      "#EXT-X-STREAM-INF:BANDWIDTH=0,RESOLUTION=3840x2160",
      "zero.m3u8",
      "#EXT-X-STREAM-INF:BANDWIDTH=not-a-number",
      "invalid.m3u8",
    ].join("\r\n");
    const base = "https://media.example/hls/master.m3u8";
    expect(parseHlsMaster(text, base)).toEqual([
      { url: "https://media.example/1080.m3u8", height: 1080, bandwidth: 8_000_000, label: "1080p · HLS" },
      { url: "https://media.example/hls/720/index.m3u8", height: 720, bandwidth: 2_000_000, label: "720p · HLS" },
    ]);
    expect(parseHlsMaster("#EXTM3U\n#EXTINF:4,\nsegment.ts", base)).toEqual([]);
  });

  it("rejects local, private and noncanonical destinations before fetching manifests", () => {
    for (const host of [
      "localhost.", "nested.localhost", "intranet", "dev.local", "0.1.2.3", "127.99.1.2", "2130706433",
      "10.12.0.3", "172.31.1.2", "192.168.1.1", "169.254.169.254", "100.127.1.2",
      "[::1]", "[::ffff:8.8.8.8]", "[fc00::1]", "[fd00::1]", "[fe90::1]",
      "[2002:7f00:1::1]", "[2001:0:4136:e378:8000:63bf:3fff:fdd2]", "[2001::1]", "[2001:db8::1]",
    ]) expect(isPublicHttpUrl("https://" + host + "/master.m3u8")).toBe(false);
    expect(isPublicHttpUrl("https://user@media.example/master.m3u8")).toBe(false);
    expect(isPublicHttpUrl("https://cdn.media.example/master.m3u8")).toBe(true);
    expect(isPublicHttpUrl("http://8.8.8.8/master.m3u8")).toBe(true);
    expect(isPublicHttpUrl("https://[2606:4700::1111]/master.m3u8")).toBe(true);
    expect(isPublicHttpUrl("https://[2001:4860:4860::8888]/master.m3u8")).toBe(true);
  });

  it("formats probed file sizes as byte units rather than transfer speeds", () => {
    const formats = probeFormats({
      title: "Demo", source_url: "https://media.example/watch", extractor: "generic",
      duration_seconds: 60, thumbnail_url: null, is_playlist: false, playlist_count: null, subtitles: [],
      formats: [
        { format_id: "137", label: "1080p MP4", container: "mp4", video_codec: "avc1", audio_codec: "aac",
          height: 1080, fps: 30, filesize_bytes: 5 * 1024 * 1024, is_audio_only: false, has_video: true, has_audio: true },
      ],
    });
    expect(formats[0]?.sub).toBe("5.0 MB");
  });
});

describe("header handoff", () => {
  const page = "https://media.example/player";
  const origin = "https://media.example";
  const stream = "https://media.example/stream.m3u8";
  const cdn = "https://cdn.media.example/stream.mp4";
  const headers = [
    { name: "cOoKiE", value: "session=test" },
    { name: "referer", value: page },
    { name: "user-agent", value: "Example Agent" },
    { name: "Authorization", value: "Bearer test" },
    { name: "Other", value: "test" },
  ];
  const expected = { Cookie: "session=test", Referer: page, "User-Agent": "Example Agent" };
  const withOrigin = [...headers, { name: "Origin", value: origin }];
  const cdnHeaders = { Referer: page, Origin: origin, "User-Agent": "Example Agent" };

  it("returns only allowed same-origin headers and checks referrer origin independently", () => {
    expect(scopedHeaders(stream, page, "https://media.example", headers)).toEqual(expected);
    expect(scopedHeaders(stream, page, page, [{ name: "Referer", value: "https://other.example/private" }])).toEqual({});
    expect(scopedHeaders(stream, page, page, [{ name: "Cookie", value: "a=b\r\nInjected: yes" }])).toEqual({});
  });

  it.each([cdn, "https://media.example:8443/stream"])(
    "passes anti-hotlink headers without credentials for %s", (url) => {
      expect(scopedHeaders(url, page, page, withOrigin)).toEqual(cdnHeaders);
      expect(scopedHeaders(url, page, "https://other.example", withOrigin)).toEqual({});
    },
  );

  it("accepts a target-origin Referer and strips its fragment", () => {
    expect(scopedHeaders(cdn, page, page, [{ name: "Referer", value: cdn + "#secret" }])).toEqual({ Referer: cdn });
    expect(scopedHeaders(cdn, page, page, [{ name: "Referer", value: "https://user:secret@media.example/player" }])).toEqual({});
  });

  it("drops page Referer, Origin and credentials on HTTPS to HTTP downgrade", () => {
    expect(scopedHeaders("http://media.example/stream", page, page, withOrigin)).toEqual({ "User-Agent": "Example Agent" });
  });

  it("isolates tabs, URL matches, redirects and navigation", () => {
    const network = new ObservedNetwork();
    const request = { tabId: 8, requestId: "1", url: stream, initiator: page };
    network.recordHeaders(request, headers);
    expect(network.headers(8, stream, page)).toEqual(expected);
    expect(network.headers(7, stream, page)).toEqual({});
    expect(network.headers(8, stream + "?other", page)).toEqual({});
    expect(network.headers(8, stream, "https://another.example")).toEqual({});
    network.recordHeaders({ ...request, url: cdn, incognito: true }, withOrigin);
    network.recordHeaders({ ...request, url: cdn, documentUrl: page, initiator: "https://other.example" }, withOrigin);
    expect(network.headers(8, cdn, page)).toEqual({});
    network.recordHeaders({ ...request, url: cdn }, withOrigin);
    expect(network.headers(8, cdn, page)).toEqual(cdnHeaders);
    expect(network.headers(8, stream, page)).toEqual({});
    expect(network.headers(8, cdn + "?different", page)).toEqual({});
    expect(network.recordStream({ ...request, url: cdn }, page, [])).toBe(true);
    expect(network.stream(8, page)?.url).toBe(cdn);
    network.clear(8);
    expect(network.stream(8, page)).toBeUndefined();
    expect(network.headers(8, cdn, page)).toEqual({});
  });

  it("scopes media headers to the frame origin and excludes Cookie and authorization", () => {
    const network = new ObservedNetwork();
    const top = "https://container.example";
    const request = { tabId: 8, frameId: 2, requestId: "1", url: stream, initiator: page };
    const values = [...headers, { name: "Origin", value: "https://media.example" }, { name: "Accept-Language", value: "en-US" }];
    network.recordHeaders(request, values, top);
    expect(network.mediaHeaders(8, stream, top, 2, page)).toEqual({
      Referer: page, Origin: "https://media.example", "User-Agent": "Example Agent", "Accept-Language": "en-US",
    });
    expect(network.mediaHeaders(8, stream, top, 3, page)).toEqual({});
    expect(network.mediaHeaders(8, stream, top, 2, top)).toEqual({});
    expect(network.mediaHeaders(8, stream, top)).toEqual({});
    expect(network.headers(8, stream, top)).toEqual({});
    expect(scopedHeaders(stream, page, page, [{ name: "Origin", value: "https://other.example" }], true)).toEqual({});
    network.recordHeaders({ ...request, frameId: 0, requestId: "2" }, values, page);
    expect(network.headers(8, stream, page)).toEqual({ ...expected, Origin: "https://media.example" });
    expect(network.mediaHeaders(8, stream, page)).not.toHaveProperty("Cookie");
    network.recordHeaders({ ...request, url: cdn }, values, top);
    expect(network.mediaHeaders(8, cdn, top, 2, page)).toEqual({ ...cdnHeaders, "Accept-Language": "en-US" });
    expect(network.mediaHeaders(8, cdn, "https://other.example", 2, page)).toEqual({});
  });

  it("isolates stream observations by frame, top origin and incognito and keeps parsed HLS variants", () => {
    const network = new ObservedNetwork();
    const top = "https://container.example";
    const request = { tabId: 8, frameId: 2, requestId: "master", url: stream, initiator: page };
    expect(network.recordStream({ ...request, incognito: true }, top, [])).toBe(false);
    expect(network.recordStream(request, top, [])).toBe(true);
    const master = network.stream(8, top, 2, page)!;
    expect(master.url).toBe(stream);
    expect(network.stream(8, top, 3, page)).toBeUndefined();
    expect(network.stream(8, top)).toBeUndefined();
    expect(network.stream(8, "https://other.example", 2, page)).toBeUndefined();
    const child = "https://media.example/720.m3u8";
    network.recordStream({ ...request, requestId: "variant", url: child }, top, []);
    expect(network.setHlsVariants(master, [{ url: child, height: 720, label: "720p · HLS" }], false)).toBe(true);
    expect(network.stream(8, top, 2, page)?.variants?.[0].url).toBe(child);
    network.recordStream({ ...request, frameId: 4, initiator: top }, top, []);
    expect(network.stream(8, top)?.frameId).toBe(4);
    expect(network.stream(8, top, 5, top)).toBeUndefined();
    network.clear(8, 4);
    expect(network.stream(8, top, 4, top)).toBeUndefined();
    expect(network.stream(8, top, 2, page)?.variants?.[0].url).toBe(child);
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

  it("intercepts extensionless URLs only when inferred MIME matches allowed extensions and never overrides explicit mismatched extensions", () => {
    expect(shouldIntercept(
      { url: "https://github.com/org/repo/releases/download/v1/app-linux", filename: "", fileSize: 5_000_000, mime: "application/octet-stream" },
      defaults,
      context,
    )).toBe(true);
    expect(shouldIntercept(
      { url: "https://sourceforge.net/projects/demo/files/latest/download", filename: "demo-setup.exe", fileSize: 2_000_000 },
      defaults,
      context,
    )).toBe(true);
    // Respects explicit filename extension allowlist rather than overriding via MIME.
    expect(shouldIntercept(
      { url: "https://cdn.example/download?id=123", filename: "package.custom", fileSize: 100_000, mime: "application/octet-stream" },
      defaults,
      context,
    )).toBe(false);
    expect(shouldIntercept(
      { url: "https://cdn.example/download?id=124", filename: "report.pdf", fileSize: 100_000, mime: "application/pdf" },
      { ...defaults, extensions: ["zip"] },
      context,
    )).toBe(false);
    expect(shouldIntercept(
      { url: "https://cdn.example/download?id=125", filename: "", fileSize: 100_000, mime: "application/pdf" },
      { ...defaults, extensions: ["zip"] },
      context,
    )).toBe(false);
    expect(shouldIntercept(
      { url: "https://cdn.example/page", filename: "index.html", fileSize: 50_000, mime: "text/html" },
      defaults,
      context,
    )).toBe(false);
    expect(shouldIntercept(
      { url: "https://cdn.example/photo", filename: "photo.png", fileSize: 50_000, mime: "image/png" },
      defaults,
      context,
    )).toBe(false);
    expect(shouldIntercept(
      { url: "http://127.0.0.1/file.zip", filename: "file.zip", fileSize: 50_000, mime: "application/zip" },
      defaults,
      context,
    )).toBe(false);
  });

  it("checks browser contexts and compares the redirect's endpoint origins", () => {
    expect(canAttributeTab({ referrer: "https://source.example/p", incognito: true }, { url: "https://source.example/p", incognito: false })).toBe(false);
    expect(canAttributeTab({ referrer: "https://source.example/p", incognito: true }, { url: "https://source.example/p" })).toBe(false);
    expect(canAttributeTab({ referrer: "https://source.example/p" }, { url: "https://source.example/p", incognito: false })).toBe(false);
    expect(canAttributeTab({ referrer: "https://source.example/p", cookieStoreId: "firefox-container-1" }, { url: "https://source.example/p", cookieStoreId: "firefox-default" })).toBe(false);
    expect(canAttributeTab({ referrer: "https://source.example/p", cookieStoreId: "firefox-default" }, { url: "https://source.example/p" })).toBe(false);
    expect(canAttributeTab({ referrer: "https://source.example/p" }, { url: "https://source.example/p", cookieStoreId: "firefox-default" })).toBe(false);
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
    const longId = "x".repeat(250);
    const shortId = "y".repeat(246);
    const boundaryProbe: MediaProbeResult = {
      ...baseProbe,
      formats: [
        { ...baseProbe.formats[0], format_id: longId },
        { ...baseProbe.formats[0], format_id: shortId },
        baseProbe.formats[3],
      ],
    };
    expect(selectPillFormatId(boundaryProbe, 2160)).toBe(`${shortId}+bestaudio`);
    expect(selectPillFormatId({ ...boundaryProbe, formats: [{ ...baseProbe.formats[0], format_id: longId }, baseProbe.formats[3]] }, 2160)).toBe(longId);
    expect(selectPillFormatId({ ...baseProbe, formats: [] }, 2160)).toBeUndefined();
  });

  it("falls back to highest format <= expectedHeight when exact resolution is absent and respects height caps and separate audio", () => {
    const hd720Probe: MediaProbeResult = {
      ...baseProbe,
      formats: [
        { format_id: "136", label: "720p H264", container: "mp4", video_codec: "avc1", audio_codec: null, height: 720, fps: 30, filesize_bytes: 1200, is_audio_only: false, has_video: true, has_audio: false },
        { format_id: "135", label: "480p H264", container: "mp4", video_codec: "avc1", audio_codec: null, height: 480, fps: 30, filesize_bytes: 800, is_audio_only: false, has_video: true, has_audio: false },
        baseProbe.formats[3],
      ],
    };
    expect(selectPillFormatId(hd720Probe, 1080)).toBe("136+bestaudio");
    expect(selectPillFormatId(hd720Probe, 2160)).toBe("136+bestaudio");
    expect(selectPillFormatId({
      ...baseProbe,
      formats: [baseProbe.formats[0]],
    }, 1080)).toBeUndefined();
    const muxedPlusVideoOnlyProbe: MediaProbeResult = {
      ...baseProbe,
      formats: [
        { format_id: "136", label: "720p video-only", container: "mp4", video_codec: "avc1", audio_codec: null, height: 720, fps: 30, filesize_bytes: 1200, is_audio_only: false, has_video: true, has_audio: false },
        baseProbe.formats[2],
      ],
    };
    expect(selectPillFormatId(muxedPlusVideoOnlyProbe, 720)).toBe("136");
    const noHeightProbe: MediaProbeResult = {
      ...baseProbe,
      formats: [
        { format_id: "mp4-default", label: "MP4", container: "mp4", video_codec: "avc1", audio_codec: "mp4a", height: null, fps: null, filesize_bytes: 1000, is_audio_only: false, has_video: true, has_audio: true },
      ],
    };
    expect(selectPillFormatId(noHeightProbe, 1080)).toBe("mp4-default");
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

  it("clears stale streamUrl across SPA navigations and ignores late A->B->A replies", () => {
    const nav = new StreamNavigationState("https://media.example/watch?v=1");
    const epochA1 = nav.epoch;
    nav.update(epochA1, "https://media.example/watch?v=1", "https://media.example/watch?v=1", "https://cdn.example/v1.m3u8");
    expect(nav.resolveTarget("blob:https://media.example/abc", "https://media.example/watch?v=1")).toBe("https://cdn.example/v1.m3u8");

    expect(nav.resolveTarget("blob:https://media.example/def", "https://media.example/watch?v=2")).toBe("https://media.example/watch?v=2");
    expect(nav.resolveTarget("blob:https://media.example/ghi", "https://media.example/watch?v=1")).toBe("https://media.example/watch?v=1");
    // Even after returning to ?v=1, an in-flight reply from the earlier visit (epochA1) is dropped.
    nav.update(epochA1, "https://media.example/watch?v=1", "https://media.example/watch?v=1", "https://cdn.example/v1.m3u8");
    expect(nav.streamUrl).toBeUndefined();
  });
});
