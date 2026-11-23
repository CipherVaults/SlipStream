use super::{Entry, EntryState, ManagerInner};
use crate::{TorrentError, TorrentVersion};
use librqbit::{api::TorrentIdOrHash, Api, TorrentStats, TorrentStatsState};
use slipstream_protocol::{TorrentFileEntry, TransferKind, TransferSnapshot, TransferStatus};

#[derive(Debug, Clone)]
pub struct TorrentStatsReport {
    pub snapshot: TransferSnapshot,
    pub initialized: bool,
    /// Completion of the selected files, which may be a subset of the torrent.
    pub finished: bool,
    pub sequential: bool,
    pub download_limit_bps: Option<u64>,
    pub upload_limit_bps: Option<u64>,
    pub seen_peers: u32,
    /// librqbit 8.1.1 does not expose peer availability or tracker seed totals.
    pub seed_count: Option<u32>,
    pub ratio: f64,
    pub completed_pieces: usize,
    pub total_pieces: usize,
}

impl Entry {
    pub(super) fn list_files(&self) -> Result<Vec<TorrentFileEntry>, TorrentError> {
        let state = self.state.lock().unwrap();
        let metadata = state.metadata.as_ref().ok_or_else(super::unresolved)?;
        let stats = state.handle.as_ref().map(|handle| handle.stats());
        let mut files = metadata.files.clone();
        for file in &mut files {
            file.selected = state.is_selected(file);
            file.downloaded_bytes = file_progress(stats.as_ref(), file);
        }
        Ok(files)
    }

    pub(super) fn report(&self, manager: &ManagerInner) -> TorrentStatsReport {
        let mut state = self.state.lock().unwrap();
        let stats = state.handle.as_ref().map(|handle| handle.stats());
        let engine_state = stats.as_ref().map(|stats| stats.state);
        let initialized = matches!(
            engine_state,
            Some(TorrentStatsState::Live | TorrentStatsState::Paused)
        );
        if initialized && !state.bitmap_synced {
            self.sync_bitmap(manager, &mut state, stats.as_ref().unwrap());
        }
        let finished = initialized && stats.as_ref().is_some_and(|stats| stats.finished);
        let metadata = state.metadata.as_ref();
        let (total_bytes, downloaded_bytes) = metadata.map_or_else(
            || {
                (
                    self.magnet.as_ref().and_then(|magnet| magnet.exact_length),
                    0,
                )
            },
            |metadata| {
                let (total, downloaded) = metadata
                    .files
                    .iter()
                    .filter(|file| state.is_selected(file))
                    .fold((0, 0), |(total, downloaded), file| {
                        (
                            total + file.size_bytes,
                            downloaded + file_progress(stats.as_ref(), file),
                        )
                    });
                (Some(total), downloaded)
            },
        );
        let error_message = state
            .error
            .clone()
            .or_else(|| stats.as_ref().and_then(|stats| stats.error.clone()));
        let bitmap = self.bitmap.read().unwrap();
        let status = if error_message.is_some() {
            TransferStatus::Failed
        } else {
            match engine_state {
                Some(TorrentStatsState::Live)
                    if finished && bitmap.completed_pieces() == bitmap.total_pieces() =>
                {
                    TransferStatus::Seeding
                }
                Some(TorrentStatsState::Live) if finished => TransferStatus::Completed,
                Some(TorrentStatsState::Live) => TransferStatus::Active,
                Some(TorrentStatsState::Paused) => TransferStatus::Paused,
                Some(TorrentStatsState::Error) => TransferStatus::Failed,
                _ if state.options.start_paused => TransferStatus::Paused,
                _ => TransferStatus::Probing,
            }
        };
        let live = stats.as_ref().and_then(|stats| stats.live.as_ref());
        let (download_speed_bps, upload_speed_bps, active_peers) = live.map_or((0, 0, 0), |live| {
            let peers = &live.snapshot.peer_stats;
            (
                (live.download_speed.mbps * 1_048_576.0) as u64,
                (live.upload_speed.mbps * 1_048_576.0) as u64,
                u32::try_from(peers.live).unwrap_or(u32::MAX),
            )
        });
        let seen_peers = state.seen_peers.max(live.map_or(0, |live| {
            u32::try_from(live.snapshot.peer_stats.seen).unwrap_or(u32::MAX)
        }));
        let uploaded_bytes = state
            .previous_uploads
            .saturating_add(stats.as_ref().map_or(0, |stats| stats.uploaded_bytes));
        let ratio = if downloaded_bytes > 0 {
            uploaded_bytes as f64 / downloaded_bytes as f64
        } else {
            0.0
        };
        let sequential = state
            .options
            .sequential
            .unwrap_or(manager.config.default_sequential);
        let version = match metadata.map(|metadata| metadata.version) {
            Some(TorrentVersion::Hybrid) => "BTv1 + BTv2",
            Some(TorrentVersion::V2) => "BTv2",
            _ => "BTv1",
        };
        let mut format_tags = vec![version.to_owned()];
        if sequential {
            format_tags.push("sequential".into());
        }
        if metadata.is_some_and(|metadata| metadata.private) {
            format_tags.push("private".into());
        }
        let filename = metadata
            .and_then(|metadata| metadata.name.clone())
            .or_else(|| {
                self.magnet
                    .as_ref()
                    .map(|magnet| magnet.safe_display_name())
            })
            .or_else(|| {
                let files = &metadata?.files;
                (files.len() == 1).then(|| files[0].relative_path.clone())
            })
            .unwrap_or_else(|| self.id.clone());
        let eta_seconds = (status == TransferStatus::Active)
            .then(|| {
                let remaining = total_bytes?.saturating_sub(downloaded_bytes);
                (download_speed_bps > 0).then(|| remaining.div_ceil(download_speed_bps))
            })
            .flatten();
        let snapshot = TransferSnapshot {
            id: self.id.clone(),
            kind: TransferKind::Torrent,
            status,
            filename,
            source_url: self.source.clone(),
            source_domain: String::new(),
            save_dir: self.save_dir.to_string_lossy().into_owned(),
            total_bytes,
            downloaded_bytes,
            uploaded_bytes,
            download_speed_bps,
            upload_speed_bps,
            eta_seconds,
            active_connections: u16::try_from(active_peers).unwrap_or(u16::MAX),
            active_peers,
            protocol_label: "BitTorrent".into(),
            format_tags,
            ui_segments: bitmap.project_to_ui_segments(),
            segments: bitmap.to_segment_snapshots(status, download_speed_bps),
            sha256_verified: None,
            resumable: true,
            queue_id: "default".into(),
            error_message,
            completion_notice: None,
            created_at: self.created_at,
            updated_at: state.updated_at,
        };
        state.seen_peers = seen_peers;
        TorrentStatsReport {
            snapshot,
            initialized,
            finished,
            sequential,
            download_limit_bps: state.options.download_limit_bps,
            upload_limit_bps: state.options.upload_limit_bps,
            seen_peers,
            seed_count: None,
            ratio,
            completed_pieces: bitmap.completed_pieces(),
            total_pieces: bitmap.total_pieces(),
        }
    }

    fn sync_bitmap(&self, manager: &ManagerInner, state: &mut EntryState, stats: &TorrentStats) {
        let id = state.handle.as_ref().unwrap().id();
        let api = Api::new(manager.session.clone(), None);
        // Initial verification doesn't invoke the storage callback. v8 exposes
        // this bitfield through api_dump_haves in bitvec's debug format.
        let dump = api.api_dump_haves(TorrentIdOrHash::Id(id)).ok();
        let bits = dump.as_deref().and_then(parse_have_bits);
        let mut bitmap = self.bitmap.write().unwrap();
        if let Some(bits) = bits.filter(|bits| bits.len() >= bitmap.total_pieces()) {
            bitmap.merge_bits(bits);
            state.bitmap_synced = true;
        } else if state.options.selected_file_indices.is_none() && stats.finished {
            let count = bitmap.total_pieces();
            bitmap.merge_bits(std::iter::repeat_n(true, count));
            state.bitmap_synced = true;
        }
    }
}

impl EntryState {
    fn is_selected(&self, file: &TorrentFileEntry) -> bool {
        file.selected
            && self
                .options
                .selected_file_indices
                .as_ref()
                .is_none_or(|indices| indices.binary_search(&file.index).is_ok())
    }
}

fn file_progress(stats: Option<&TorrentStats>, file: &TorrentFileEntry) -> u64 {
    stats
        .and_then(|stats| stats.file_progress.get(file.index))
        .copied()
        .unwrap_or(0)
        .min(file.size_bytes)
}

fn parse_have_bits(dump: &str) -> Option<Vec<bool>> {
    let list = dump.rsplit_once('[')?.1.strip_suffix(']')?;
    list.split(',')
        .map(|bit| match bit.trim() {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        })
        .collect()
}
