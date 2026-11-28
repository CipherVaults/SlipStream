use crate::paths::{
    delete_open_file, file_identity, local_drive_path, open_local_directory, open_local_file,
    validate_existing_ancestors, FileIdentity,
};
use chrono::Weekday;
use slipstream_protocol::{
    default_alt_speed_limit_bps, AppSettings, AutomationStatus, ScheduleMode, ScheduleSettings,
    WatchFolderRule,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationDecision {
    pub status: AutomationStatus,
    pub should_pause_transfers: bool,
    pub stop_torrents: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchFolderCandidate {
    pub rule_id: String,
    pub torrent_path: PathBuf,
    pub target_save_dir: Option<String>,
    pub queue_id: Option<String>,
    pub category_id: Option<String>,
    pub delete_torrent_after_add: bool,
    pub start_paused: bool,
}

pub(crate) struct CapturedWatchFile {
    pub bytes: Vec<u8>,
    pub size: u64,
    pub modified: SystemTime,
    identity: FileIdentity,
}

impl CapturedWatchFile {
    pub(crate) fn delete_after_add(&self, path: &Path) -> io::Result<()> {
        let file = open_local_file(path, true)?;
        if file_identity(&file)? != (self.identity, 1)
            || file.metadata()?.len() != self.size
            || file.metadata()?.modified()? != self.modified
        {
            return Err(io::Error::other(
                "watch folder file changed after it was read",
            ));
        }
        delete_open_file(&file)
    }
}

const MAX_TORRENT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_WATCH_ATTEMPTS: u8 = 4; // Initial attempt and three retries.

#[derive(Default)]
pub struct WatchFolderTracker {
    seen: HashMap<PathBuf, WatchFile>,
    captured: HashMap<PathBuf, CapturedWatchFile>,
}

struct WatchFile {
    size: u64,
    modified: SystemTime,
    added: bool,
    failures: u8,
}

impl WatchFolderTracker {
    pub fn mark_seen(&mut self, path: PathBuf, size: u64, modified: SystemTime) {
        self.mark_added(path, size, modified);
    }

    pub fn mark_added(&mut self, path: PathBuf, size: u64, modified: SystemTime) {
        self.seen.insert(
            path,
            WatchFile {
                size,
                modified,
                added: true,
                failures: 0,
            },
        );
    }

    pub fn record_failure(&mut self, path: &Path) {
        if let Some(entry) = self.seen.get_mut(path) {
            entry.failures = entry.failures.saturating_add(1);
        }
    }

    pub fn forget(&mut self, path: &Path) {
        self.seen.remove(path);
        self.captured.remove(path);
    }

    pub(crate) fn take_captured(&mut self, path: &Path) -> Option<CapturedWatchFile> {
        self.captured.remove(path)
    }

    fn should_try(&mut self, path: &Path, size: u64, modified: SystemTime) -> bool {
        let entry = self.seen.entry(path.to_path_buf()).or_insert(WatchFile {
            size,
            modified,
            added: false,
            failures: 0,
        });
        if (entry.size, entry.modified) != (size, modified) {
            *entry = WatchFile {
                size,
                modified,
                added: false,
                failures: 0,
            };
        }
        !entry.added && entry.failures < MAX_WATCH_ATTEMPTS
    }
}

pub fn is_within_window(minute_of_day: u16, weekday: Weekday, schedule: &ScheduleSettings) -> bool {
    if schedule.mode == ScheduleMode::Disabled {
        return false;
    }
    if schedule.weekdays_only && matches!(weekday, Weekday::Sat | Weekday::Sun) {
        return false;
    }
    let now = minute_of_day % 1440;
    let start = schedule.start_minute_of_day % 1440;
    let end = schedule.end_minute_of_day % 1440;
    if start == end {
        return true;
    }
    if start < end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

pub fn compute_effective_decision(
    settings: &AppSettings,
    minute_of_day: u16,
    weekday: Weekday,
    fullscreen_active: bool,
    matched_process: Option<&str>,
    active_queue_actions: usize,
) -> AutomationDecision {
    let schedule_window_active = is_within_window(minute_of_day, weekday, &settings.schedule);

    let guard = &settings.bandwidth_guard;
    let (guard_active, guard_reason) = if guard.enabled {
        if let Some(proc_name) = matched_process {
            (true, Some(format!("Process active: {proc_name}")))
        } else if guard.throttle_on_fullscreen && fullscreen_active {
            (true, Some("Fullscreen app detected".to_owned()))
        } else {
            (false, None)
        }
    } else {
        (false, None)
    };

    let mut caps: Vec<u64> = Vec::new();
    if let Some(global) = settings.global_speed_limit_bps.filter(|&r| r > 0) {
        caps.push(global);
    }
    if settings.alt_speed_enabled {
        if let Some(alt) = settings
            .alt_speed_limit_bps
            .or_else(default_alt_speed_limit_bps)
            .filter(|&r| r > 0)
        {
            caps.push(alt);
        }
    }
    if settings.schedule.mode == ScheduleMode::SnailWindow && schedule_window_active {
        if let Some(win_cap) = settings.schedule.window_speed_limit_bps.filter(|&r| r > 0) {
            caps.push(win_cap);
        }
    }
    if guard_active && !guard.pause_instead_of_throttle && guard.guard_speed_limit_bps > 0 {
        caps.push(guard.guard_speed_limit_bps);
    }

    let effective_speed_limit_bps = caps.into_iter().min();
    let paused_by_guard = guard_active && guard.pause_instead_of_throttle;
    let paused_by_schedule = settings.schedule.mode == ScheduleMode::OffPeakWindow
        && !schedule_window_active
        && settings.schedule.post_window_pause;
    let stop_torrents = settings.schedule.mode != ScheduleMode::Disabled
        && !schedule_window_active
        && settings.schedule.stop_torrents_at_end;

    AutomationDecision {
        status: AutomationStatus {
            alt_speed_active: settings.alt_speed_enabled,
            effective_speed_limit_bps,
            schedule_window_active,
            guard_active,
            guard_reason,
            active_queue_actions,
            ..Default::default()
        },
        should_pause_transfers: paused_by_guard || paused_by_schedule,
        stop_torrents,
    }
}

pub fn scan_watch_folders(
    rules: &[WatchFolderRule],
    tracker: &mut WatchFolderTracker,
) -> Vec<WatchFolderCandidate> {
    tracker.captured.clear();
    let mut found = Vec::new();
    let mut seen_this_scan = HashSet::new();
    let mut remaining = MAX_TORRENT_BYTES;
    for rule in rules {
        if !rule.enabled || rule.folder.trim().is_empty() {
            continue;
        }
        let Ok(clean_folder) = local_drive_path(Path::new(&rule.folder)) else {
            continue;
        };
        if validate_existing_ancestors(&clean_folder).is_err() {
            continue;
        }
        let Ok(_folder) = open_local_directory(&clean_folder) else {
            continue;
        };
        let Ok(entries) = fs::read_dir(&clean_folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("torrent"))
                || !seen_this_scan.insert(path.clone())
            {
                continue;
            }
            let Ok(file) = open_local_file(&path, false) else {
                continue;
            };
            let Ok(meta) = file.metadata() else {
                continue;
            };
            let size = meta.len();
            let Ok(modified) = meta.modified() else {
                continue;
            };
            if size == 0
                || size > MAX_TORRENT_BYTES
                || size > remaining
                || !tracker.should_try(&path, size, modified)
            {
                continue;
            }
            let Ok((identity, _)) = file_identity(&file) else {
                tracker.record_failure(&path);
                continue;
            };
            let mut bytes = Vec::new();
            if (&file).take(size + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 != size
            {
                tracker.record_failure(&path);
                continue;
            }
            remaining -= size;
            tracker.captured.insert(
                path.clone(),
                CapturedWatchFile {
                    bytes,
                    size,
                    modified,
                    identity,
                },
            );
            found.push(WatchFolderCandidate {
                rule_id: rule.id.clone(),
                torrent_path: path,
                target_save_dir: rule.target_save_dir.clone(),
                queue_id: rule.queue_id.clone(),
                category_id: rule.category_id.clone(),
                delete_torrent_after_add: rule.delete_torrent_after_add,
                start_paused: rule.start_paused,
            });
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static WATCH_TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "slipstream-automation-test-{}-{}",
                std::process::id(),
                WATCH_TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn schedule_window_handles_normal_overnight_and_weekdays_only() {
        let normal = ScheduleSettings {
            mode: ScheduleMode::OffPeakWindow,
            start_minute_of_day: 9 * 60,
            end_minute_of_day: 17 * 60,
            weekdays_only: true,
            ..ScheduleSettings::default()
        };
        assert!(is_within_window(9 * 60, Weekday::Mon, &normal));
        assert!(is_within_window(16 * 60 + 59, Weekday::Fri, &normal));
        assert!(!is_within_window(17 * 60, Weekday::Fri, &normal));
        assert!(!is_within_window(12 * 60, Weekday::Sat, &normal));

        let overnight = ScheduleSettings {
            mode: ScheduleMode::SnailWindow,
            start_minute_of_day: 23 * 60,
            end_minute_of_day: 6 * 60,
            weekdays_only: false,
            ..ScheduleSettings::default()
        };
        assert!(is_within_window(23 * 60 + 30, Weekday::Sun, &overnight));
        assert!(is_within_window(0, Weekday::Mon, &overnight));
        assert!(is_within_window(5 * 60 + 59, Weekday::Tue, &overnight));
        assert!(!is_within_window(6 * 60, Weekday::Tue, &overnight));
        assert!(!is_within_window(14 * 60, Weekday::Tue, &overnight));
    }

    #[test]
    fn decision_picks_strictest_speed_cap_and_handles_pause_modes() {
        let mut settings = AppSettings {
            global_speed_limit_bps: Some(10 * 1024 * 1024),
            alt_speed_enabled: true,
            alt_speed_limit_bps: Some(512 * 1024),
            schedule: ScheduleSettings {
                mode: ScheduleMode::SnailWindow,
                start_minute_of_day: 22 * 60,
                end_minute_of_day: 7 * 60,
                weekdays_only: false,
                window_speed_limit_bps: Some(256 * 1024),
                ..ScheduleSettings::default()
            },
            ..AppSettings::default()
        };
        settings.bandwidth_guard.enabled = true;
        settings.bandwidth_guard.guard_speed_limit_bps = 128 * 1024;
        settings.bandwidth_guard.pause_instead_of_throttle = false;

        let d = compute_effective_decision(&settings, 23 * 60, Weekday::Wed, true, None, 1);
        assert!(d.status.alt_speed_active);
        assert!(d.status.schedule_window_active);
        assert!(d.status.guard_active);
        assert_eq!(d.status.effective_speed_limit_bps, Some(128 * 1024));
        assert!(!d.should_pause_transfers);

        settings.bandwidth_guard.pause_instead_of_throttle = true;
        let d2 = compute_effective_decision(
            &settings,
            12 * 60,
            Weekday::Wed,
            false,
            Some("zoom.exe"),
            0,
        );
        assert!(d2.status.guard_active);
        assert!(d2.should_pause_transfers);
        assert_eq!(
            d2.status.guard_reason.as_deref(),
            Some("Process active: zoom.exe")
        );
        // Pause mode removes guard throttle from rate caps cleanly
        assert_eq!(d2.status.effective_speed_limit_bps, Some(512 * 1024));
    }

    #[test]
    fn watch_folder_scanner_dedups_torrents_and_skips_junctions() {
        let dir = TempDir::new();
        let watch = dir.0.join("watch");
        let outside = dir.0.join("outside");
        let junction = dir.0.join("watch_junction");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(watch.join("ubuntu.torrent"), b"d4:infod4:name6:ubuntuee").unwrap();
        fs::write(watch.join("ignore.txt"), b"hello").unwrap();
        fs::write(outside.join("evil.torrent"), b"d4:infod4:name4:evilee").unwrap();

        #[cfg(windows)]
        {
            let status = Command::new("cmd")
                .args([
                    "/C",
                    "mklink",
                    "/J",
                    &junction.to_string_lossy(),
                    &outside.to_string_lossy(),
                ])
                .output()
                .unwrap();
            assert!(status.status.success());
        }

        let rules = vec![
            WatchFolderRule {
                id: "main".into(),
                folder: watch.to_string_lossy().into_owned(),
                target_save_dir: None,
                queue_id: Some("bulk".into()),
                category_id: None,
                enabled: true,
                delete_torrent_after_add: true,
                start_paused: false,
            },
            WatchFolderRule {
                id: "evil".into(),
                folder: junction.to_string_lossy().into_owned(),
                target_save_dir: None,
                queue_id: None,
                category_id: None,
                enabled: true,
                delete_torrent_after_add: false,
                start_paused: false,
            },
        ];

        let mut tracker = WatchFolderTracker::default();
        let first = scan_watch_folders(&rules, &mut tracker);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].rule_id, "main");
        assert!(first[0].delete_torrent_after_add);

        let candidate = &first[0];
        let captured = tracker.take_captured(&candidate.torrent_path).unwrap();
        assert_eq!(captured.bytes, b"d4:infod4:name6:ubuntuee");
        for _ in 0..MAX_WATCH_ATTEMPTS {
            assert_eq!(scan_watch_folders(&rules, &mut tracker).len(), 1);
            tracker.record_failure(&candidate.torrent_path);
        }
        assert!(scan_watch_folders(&rules, &mut tracker).is_empty());
        fs::write(&candidate.torrent_path, b"changed").unwrap();
        let changed = scan_watch_folders(&rules, &mut tracker);
        assert_eq!(changed.len(), 1);
        let path = changed[0].torrent_path.clone();
        let captured = tracker.take_captured(&path).unwrap();
        tracker.mark_added(path, captured.size, captured.modified);
        let second = scan_watch_folders(&rules, &mut tracker);
        assert!(second.is_empty());
    }
}
