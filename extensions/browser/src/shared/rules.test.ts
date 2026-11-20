import { describe, expect, it } from "vitest";
import { defaults, shouldIntercept } from "./downloads";
import { ObservedNetwork } from "./network";
import { isMediaStream, mediaMime, scopedHeaders } from "./streams";

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
});
