import type { ToolSource } from "@slipstream/ipc-types";

export * from "@slipstream/ipc-types";

export interface RefreshDownloadUrlRequest {
  transfer_id: string;
  new_url: string;
  headers: Record<string, string>;
  referrer?: string | null;
}

export interface ToolchainStatus {
  ytdlp_path: string | null;
  ytdlp_version: string | null;
  ytdlp_source: ToolSource;
  ffmpeg_path: string | null;
  ffmpeg_version: string | null;
  ffmpeg_source: ToolSource;
  ffprobe_path: string | null;
  ffprobe_version?: string | null;
  ffprobe_source: ToolSource;
  native_hosts_installed: boolean;
}
