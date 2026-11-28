export const NATIVE_HOST_NAME = "com.ciphervaults.slipstream";
export const DEFAULT_CHROMIUM_EXTENSION_ID = "nikdbkenglojniombimjdkneanmponcp";
export const DEFAULT_FIREFOX_EXTENSION_ID = "slipstream@ciphervaults.com";

export type TransferKind = "http" | "media" | "torrent";

export type TransferStatus =
  | "queued"
  | "probing"
  | "active"
  | "remuxing"
  | "paused"
  | "seeding"
  | "completed"
  | "failed"
  | "canceled";

export type SegmentState = "pending" | "active" | "split" | "completed" | "paused" | "error";

export interface SegmentSnapshot {
  index: number;
  start_byte: number;
  end_byte: number;
  downloaded_bytes: number;
  speed_bps: number;
  state: SegmentState;
}

export interface TransferSnapshot {
  id: string;
  kind: TransferKind;
  status: TransferStatus;
  filename: string;
  source_url: string;
  source_domain: string;
  save_dir: string;
  total_bytes: number | null;
  downloaded_bytes: number;
  uploaded_bytes: number;
  download_speed_bps: number;
  upload_speed_bps: number;
  eta_seconds: number | null;
  active_connections: number;
  active_peers: number;
  protocol_label: string;
  format_tags: string[];
  ui_segments: number[];
  segments: SegmentSnapshot[];
  sha256_verified: boolean | null;
  resumable: boolean;
  queue_id: string;
  error_message: string | null;
  completion_notice?: string | null;
  playlist_id?: string | null;
  playlist_title?: string | null;
  playlist_paused?: boolean | null;
  playlist_index?: number | null;
  playlist_total?: number | null;
  speed_limit_bps?: number | null;
  connect_rtt_ms?: number | null;
  description?: string | null;
  category_id?: string | null;
  tags?: string[];
  created_at: string;
  updated_at: string;
}

export interface TelemetryBatch {
  aggregate_down_bps: number;
  aggregate_up_bps: number;
  active_count: number;
  active_threads: number;
  speed_limit_bps: number | null;
  transfers: TransferSnapshot[];
  timestamp: string;
}

export interface AddHttpDownloadRequest {
  url: string;
  filename?: string | null;
  save_dir?: string | null;
  max_connections?: number | null;
  queue_id?: string | null;
  headers: Record<string, string>;
  referrer?: string | null;
  expected_sha256?: string | null;
  expected_size_bytes?: number | null;
  start_paused?: boolean;
  overwrite_existing?: boolean;
  mirror_urls?: string[];
  auto_discover_checksum?: boolean;
}

export type AudioPreset = "mp3_320" | "m4a_aac" | "opus" | "flac" | "wav";

export type MediaContainer = "mp4" | "mkv" | "webm" | "mp3" | "m4a" | "flac" | "opus" | "wav";

export interface MediaFormatOption {
  format_id: string;
  label: string;
  container: string;
  video_codec: string | null;
  audio_codec: string | null;
  height: number | null;
  fps: number | null;
  filesize_bytes: number | null;
  bitrate_kbps?: number | null;
  audio_track_kind?: "descriptive" | "dubbed" | "unspecified" | "default" | "original";
  is_audio_only: boolean;
  has_video: boolean;
  has_audio: boolean;
}

export interface SubtitleTrack {
  lang: string;
  name: string;
  is_auto: boolean;
}

export interface PlaylistEntry {
  /** 1-based index within the playlist. */
  index: number;
  id?: string | null;
  title: string;
  url?: string | null;
  duration_seconds?: number | null;
  thumbnail_url?: string | null;
}

export interface MediaProbeResult {
  title: string;
  source_url: string;
  extractor: string;
  duration_seconds: number | null;
  thumbnail_url: string | null;
  is_playlist: boolean;
  playlist_count: number | null;
  playlist_entries?: string[];
  playlist_items?: PlaylistEntry[];
  formats: MediaFormatOption[];
  subtitles: SubtitleTrack[];
}

export interface SelectedPlaylistEntry {
  /** 1-based index within the playlist. */
  index: number;
  title: string;
  url?: string | null;
}

export interface AddMediaDownloadRequest {
  url: string;
  format_id?: string | null;
  audio_only_preset?: AudioPreset | null;
  target_container?: MediaContainer | null;
  embed_subtitles_langs: string[];
  lossless_remux: boolean;
  normalize_audio: boolean;
  save_dir?: string | null;
  filename?: string | null;
  playlist_items?: string | null;
  download_playlist?: boolean;
  max_height?: number | null;
  selected_entries?: SelectedPlaylistEntry[] | null;
  playlist_folder_name?: string | null;
  headers?: Record<string, string>;
  clip_start_seconds?: number | null;
  clip_end_seconds?: number | null;
  split_chapters?: boolean;
  sponsorblock_categories?: string[];
}

export interface TorrentFileEntry {
  index: number;
  relative_path: string;
  size_bytes: number;
  downloaded_bytes: number;
  selected: boolean;
}

export interface AddTorrentRequest {
  source: string;
  save_dir?: string | null;
  selected_file_indices?: number[] | null;
  sequential: boolean;
  start_paused?: boolean;
}

export type ThemeMode = "system" | "dark" | "light";
export type AccentMode = "blue" | "mono";
export type UpdateMode = "auto" | "manual";
export type UpdateProgressStage = "downloading" | "verifying" | "installing";
export type ToolSource = "custom" | "managed" | "bundled" | "system_path" | "missing";
export type BrowserKind = "chrome" | "edge" | "brave" | "firefox";

export interface UpdateProgress {
  stage: UpdateProgressStage;
  downloaded_bytes: number;
  total_bytes?: number | null;
}

export interface BrowserStatus {
  id: BrowserKind;
  name: string;
  installed: boolean;
  native_host_registered: boolean;
  extension_url: string;
  bundled_extension_dir: string | null;
  extension_id: string;
}

export interface BrowserSetupResult {
  browser_id: BrowserKind;
  launched_browser: boolean;
  revealed_extension_dir: boolean;
  native_host_registered: boolean;
  extension_dir: string;
  extension_page_url: string;
  extension_id: string;
  instructions: string;
}

export interface UpdateStatus {
  available: boolean;
  current_version: string;
  version?: string | null;
  notes?: string | null;
  pub_date?: string | null;
}

export interface DownloadCategory {
  id: string;
  name: string;
  extensions: string[];
  save_dir?: string | null;
}

export interface DuplicateMatch {
  transfer_id: string;
  filename: string;
  status: TransferStatus;
  save_dir: string;
  total_bytes: number | null;
  downloaded_bytes: number;
}

export interface PendingDownloadPreview {
  pending_id: string;
  url: string;
  source_domain: string;
  filename: string;
  suggested_category_id: string;
  suggested_save_dir: string;
  expected_size_bytes: number | null;
  duplicate: DuplicateMatch | null;
}

export interface ConfirmPendingDownloadRequest {
  pending_id: string;
  filename: string;
  save_dir: string;
  category_id: string;
  remember_category_path: boolean;
  start_immediately: boolean;
  overwrite_existing: boolean;
  description?: string | null;
}

export interface TransferCompletionOptions {
  show_complete_dialog: boolean;
  auto_close_progress: boolean;
  open_folder_on_complete: boolean;
}

export type PostQueueAction = "none" | "notify" | "sleep" | "shutdown";

export type ScheduleMode = "disabled" | "off_peak_window" | "snail_window";

export interface ScheduleSettings {
  mode: ScheduleMode;
  start_minute_of_day: number;
  end_minute_of_day: number;
  weekdays_only: boolean;
  window_speed_limit_bps: number | null;
  post_window_pause: boolean;
  stop_torrents_at_end: boolean;
}

export interface BandwidthGuardSettings {
  enabled: boolean;
  throttle_on_fullscreen: boolean;
  watched_processes: string[];
  guard_speed_limit_bps: number;
  pause_instead_of_throttle: boolean;
}

export interface DownloadQueueSpec {
  id: string;
  name: string;
  max_concurrent: number;
  speed_limit_bps: number | null;
  post_action: PostQueueAction;
  paused: boolean;
}

export interface WatchFolderRule {
  id: string;
  folder: string;
  target_save_dir: string | null;
  queue_id: string | null;
  category_id: string | null;
  enabled: boolean;
  delete_torrent_after_add: boolean;
  start_paused: boolean;
}

export interface AutoExtractSettings {
  enabled: boolean;
  create_subfolder: boolean;
  delete_archive_after_extract: boolean;
  overwrite_existing: boolean;
}

export interface TorrentNetworkSettings {
  listen_port_start: number;
  listen_port_end: number;
  enable_dht: boolean;
  enable_upnp: boolean;
  global_upload_limit_bps: number | null;
  socks5_proxy_url: string | null;
  blocklist_path: string | null;
  vpn_kill_switch_enabled: boolean;
  vpn_adapter_name: string | null;
}

export interface TorrentSeedingSettings {
  max_seed_ratio_hundredths: number | null;
  max_seed_minutes: number | null;
  stop_seeding_on_limit: boolean;
}

export interface RssFeedRule {
  id: string;
  name: string;
  feed_url: string;
  include_pattern: string;
  exclude_pattern: string;
  save_dir: string | null;
  poll_interval_minutes: number;
  enabled: boolean;
  auto_start: boolean;
  last_checked_at: string | null;
  last_error: string | null;
}

export interface CreateTorrentRequest {
  source_path: string;
  output_path: string;
  trackers: string[];
  piece_length_kib: number;
  private: boolean;
  comment: string | null;
}

export interface CreateTorrentResult {
  output_path: string;
  info_hash: string;
  total_bytes: number;
  piece_count: number;
  file_count: number;
}

export interface AutomationStatus {
  alt_speed_active: boolean;
  effective_speed_limit_bps: number | null;
  schedule_window_active: boolean;
  guard_active: boolean;
  guard_reason: string | null;
  active_queue_actions: number;
  vpn_kill_switch_active?: boolean;
  vpn_adapter_connected?: boolean;
  vpn_adapter_name?: string | null;
  rss_feeds_enabled?: number;
  rss_matches_added?: number;
}

export type ProxyMode = "direct" | "system" | "manual";

export interface ProxySettings {
  mode: ProxyMode;
  url: string | null;
  username: string | null;
  has_password: boolean;
  bypass_hosts: string[];
  apply_to_media: boolean;
}

export type SiteAuthScheme = "basic" | "bearer";

export interface SiteLoginEntry {
  id: string;
  host_pattern: string;
  username: string;
  has_password: boolean;
  auth_scheme: SiteAuthScheme;
  enabled: boolean;
}

export interface CookiePassThroughSettings {
  enabled: boolean;
  browser: string;
  allowed_hosts: string[];
}

export interface MediaPowerSettings {
  sponsorblock_remove: string[];
  embed_chapters: boolean;
  split_chapters: boolean;
  embed_thumbnail: boolean;
  embed_metadata: boolean;
  filename_template: string;
}

export interface SiteGrabRequest {
  url: string;
  max_depth: number;
  same_origin_only: boolean;
  extensions: string[];
  include_pattern: string;
  exclude_pattern: string;
}

export interface SiteGrabItem {
  url: string;
  filename: string;
  kind: string;
  source_page: string;
}

export interface SiteGrabResult {
  scanned_pages: number;
  items: SiteGrabItem[];
}

export interface MetalinkFileEntry {
  filename: string;
  size_bytes: number | null;
  sha256: string | null;
  mirrors: string[];
}

export interface AppSettings {
  theme: ThemeMode;
  accent: AccentMode;
  default_download_dir: string;
  default_connections: number;
  global_speed_limit_bps: number | null;
  max_concurrent_downloads: number;
  auto_defender_scan: boolean;
  close_to_tray_hibernate: boolean;
  monitor_clipboard: boolean;
  ytdlp_path: string | null;
  ffmpeg_path: string | null;
  update_mode?: UpdateMode;
  show_download_dialog?: boolean;
  show_progress_dialog?: boolean;
  show_complete_dialog?: boolean;
  categories?: DownloadCategory[];
  launch_on_startup?: boolean;
  launch_minimized?: boolean;
  notify_on_complete?: boolean;
  sound_on_complete?: boolean;
  show_drop_basket?: boolean;
  compact_table_view?: boolean;
  sort_column?: string;
  sort_ascending?: boolean;
  alt_speed_enabled?: boolean;
  alt_speed_limit_bps?: number | null;
  schedule?: ScheduleSettings;
  bandwidth_guard?: BandwidthGuardSettings;
  queues?: DownloadQueueSpec[];
  watch_folders?: WatchFolderRule[];
  auto_extract?: AutoExtractSettings;
  torrent_network?: TorrentNetworkSettings;
  torrent_seeding?: TorrentSeedingSettings;
  rss_feeds?: RssFeedRule[];
  proxy?: ProxySettings;
  site_logins?: SiteLoginEntry[];
  cookie_passthrough?: CookiePassThroughSettings;
  media_power?: MediaPowerSettings;
  auto_discover_checksums?: boolean;
}

export interface SettingsBackupBundle {
  schema_version: number;
  exported_at_epoch_ms: number;
  app_version: string;
  settings: AppSettings;
}

export type NativeBridgeRequest =
  | { type: "authenticate"; token: string; client_id: string }
  | { type: "ping" }
  | { type: "get_status" }
  | { type: "add_http_download"; request: AddHttpDownloadRequest }
  | { type: "probe_media"; url: string; playlist?: boolean | null; headers?: Record<string, string> }
  | { type: "add_media_download"; request: AddMediaDownloadRequest }
  | { type: "add_torrent"; request: AddTorrentRequest }
  | {
      type: "refresh_download_url";
      request: {
        transfer_id: string;
        new_url: string;
        headers: Record<string, string>;
        referrer?: string | null;
      };
    }
  | { type: "pause_transfer"; transfer_id: string }
  | { type: "resume_transfer"; transfer_id: string }
  | { type: "open_desktop_window"; open_picker_url?: string | null; headers?: Record<string, string> }
  | { type: "grab_batch_urls"; urls: string[]; referrer?: string | null };

export type NativeBridgeResponse =
  | { type: "ack"; ok: boolean; message: string | null }
  | { type: "status"; batch: TelemetryBatch; theme: ThemeMode; accent: AccentMode }
  | { type: "transfer_created"; transfer_id: string }
  | { type: "media_probed"; probe: MediaProbeResult }
  | { type: "error"; code: string; message: string };
