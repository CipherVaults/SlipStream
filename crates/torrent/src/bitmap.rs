use slipstream_protocol::{SegmentSnapshot, SegmentState, TransferStatus, UI_SEGMENT_COUNT};

#[derive(Debug, Clone)]
pub struct PieceBitmap {
    piece_length: u64,
    total_bytes: u64,
    completed: Vec<bool>,
    completed_pieces: usize,
    completed_bytes: u64,
    segment_bytes: [u64; UI_SEGMENT_COUNT],
    // Equal UI ranges may include fractions of a byte. Units are 1/16 of a byte.
    segment_units: [u128; UI_SEGMENT_COUNT],
}

impl PieceBitmap {
    pub fn new(total_pieces: usize, piece_length: u64, total_bytes: u64) -> Self {
        Self {
            piece_length,
            total_bytes,
            completed: vec![false; total_pieces],
            completed_pieces: 0,
            completed_bytes: 0,
            segment_bytes: [0; UI_SEGMENT_COUNT],
            segment_units: [0; UI_SEGMENT_COUNT],
        }
    }

    pub fn total_pieces(&self) -> usize {
        self.completed.len()
    }

    pub fn piece_byte_length(&self, piece_index: usize) -> Option<u64> {
        self.piece_bounds(piece_index)
            .map(|(start, end)| end - start)
    }

    fn piece_bounds(&self, piece_index: usize) -> Option<(u64, u64)> {
        (piece_index < self.total_pieces()).then(|| {
            let start = (piece_index as u64)
                .saturating_mul(self.piece_length)
                .min(self.total_bytes);
            let end = start
                .saturating_add(self.piece_length)
                .min(self.total_bytes);
            (start, end)
        })
    }

    pub fn mark_completed(&mut self, piece_index: usize) -> bool {
        let Some((start, end)) = self.piece_bounds(piece_index) else {
            return false;
        };
        if std::mem::replace(&mut self.completed[piece_index], true) {
            return false;
        }
        self.completed_pieces += 1;
        self.completed_bytes += end - start;
        if start == end {
            return true;
        }
        let scale = UI_SEGMENT_COUNT as u128;
        let total = u128::from(self.total_bytes);
        let scaled_start = u128::from(start) * scale;
        let scaled_end = u128::from(end) * scale;
        let first = (scaled_start / total) as usize;
        // Integer reporting rounds each segment endpoint down, so it can overlap
        // the segment immediately after the last fractional overlap.
        let last = ((scaled_end / total) as usize).min(UI_SEGMENT_COUNT - 1);
        for index in first..=last {
            let (segment_start, segment_end) = self.segment_bounds(index);
            self.segment_bytes[index] += segment_end
                .min(end)
                .saturating_sub(segment_start.max(start));
            let unit_start = total * index as u128;
            let unit_end = unit_start + total;
            self.segment_units[index] += unit_end
                .min(scaled_end)
                .saturating_sub(unit_start.max(scaled_start));
        }
        true
    }

    pub fn set_bits(&mut self, bits: &[bool]) {
        self.completed.fill(false);
        self.completed_pieces = 0;
        self.completed_bytes = 0;
        self.segment_bytes.fill(0);
        self.segment_units.fill(0);
        self.merge_bits(bits.iter().copied());
    }

    pub(crate) fn merge_bits(&mut self, bits: impl IntoIterator<Item = bool>) {
        for (index, complete) in bits.into_iter().take(self.total_pieces()).enumerate() {
            if complete {
                self.mark_completed(index);
            }
        }
    }

    pub fn completed_pieces(&self) -> usize {
        self.completed_pieces
    }

    pub fn completed_bytes(&self) -> u64 {
        self.completed_bytes
    }

    pub fn completion_fraction(&self) -> f32 {
        if self.total_bytes == 0 {
            0.0
        } else {
            (self.completed_bytes as f64 / self.total_bytes as f64) as f32
        }
    }

    pub fn project_to_ui_segments(&self) -> [f32; UI_SEGMENT_COUNT] {
        if self.total_bytes == 0 {
            return [0.0; UI_SEGMENT_COUNT];
        }
        self.segment_units
            .map(|units| (units as f64 / self.total_bytes as f64) as f32)
    }

    /// Returns integer byte ranges with an exclusive `end_byte`.
    pub fn to_segment_snapshots(
        &self,
        status: TransferStatus,
        download_speed_bps: u64,
    ) -> Vec<SegmentSnapshot> {
        let downloading = status == TransferStatus::Active;
        let pending_segments = self
            .segment_units
            .iter()
            .filter(|&&units| units < u128::from(self.total_bytes))
            .count() as u64;
        let mut remaining_speed = download_speed_bps;
        let mut remaining_segments = pending_segments;
        (0..UI_SEGMENT_COUNT)
            .map(|index| {
                let (start_byte, end_byte) = self.segment_bounds(index);
                let complete = self.total_bytes != 0
                    && self.segment_units[index] == u128::from(self.total_bytes);
                let (state, speed_bps) = if complete {
                    (SegmentState::Completed, 0)
                } else if downloading && remaining_segments > 0 {
                    let speed = remaining_speed / remaining_segments;
                    remaining_segments -= 1;
                    remaining_speed -= speed;
                    (SegmentState::Active, speed)
                } else {
                    let state = match status {
                        TransferStatus::Paused => SegmentState::Paused,
                        TransferStatus::Failed => SegmentState::Error,
                        _ => SegmentState::Pending,
                    };
                    (state, 0)
                };
                SegmentSnapshot {
                    index: index as u8,
                    start_byte,
                    end_byte,
                    downloaded_bytes: self.segment_bytes[index],
                    speed_bps,
                    state,
                }
            })
            .collect()
    }

    fn segment_bounds(&self, index: usize) -> (u64, u64) {
        let boundary =
            |i| (u128::from(self.total_bytes) * i as u128 / UI_SEGMENT_COUNT as u128) as u64;
        (boundary(index), boundary(index + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_exact_byte_overlaps_for_all_layout_sizes() {
        assert_eq!(
            PieceBitmap::new(0, 0, 0).project_to_ui_segments(),
            [0.0; 16]
        );
        for (pieces, length, total) in [(1, 16, 1), (3, 10, 29), (16, 4, 63), (41, 3, 121)] {
            let mut bitmap = PieceBitmap::new(pieces, length, total);
            let bits: Vec<_> = (0..pieces).map(|i| i % 2 == 0).collect();
            bitmap.set_bits(&bits);
            let projection = bitmap.project_to_ui_segments();
            let snapshots = bitmap.to_segment_snapshots(TransferStatus::Active, 777);
            for segment in snapshots.iter() {
                let downloaded = (segment.start_byte..segment.end_byte)
                    .filter(|byte| bits[(byte / length) as usize])
                    .count() as u64;
                assert_eq!(segment.downloaded_bytes, downloaded);
                let idx = u64::from(segment.index);
                let units = (idx * total..(idx + 1) * total)
                    .filter(|unit| bits[(unit / 16 / length) as usize])
                    .count();
                assert_eq!(
                    projection[idx as usize],
                    (units as f64 / total as f64) as f32
                );
            }
            assert_eq!(
                snapshots
                    .iter()
                    .map(|segment| segment.speed_bps)
                    .sum::<u64>(),
                if pieces == 1 { 0 } else { 777 }
            );
            assert_eq!(
                bitmap.piece_byte_length(pieces - 1),
                Some(total - (pieces as u64 - 1) * length)
            );
            assert!(!bitmap.mark_completed(0));
            assert!(!bitmap.mark_completed(pieces));
            bitmap.set_bits(&vec![true; pieces]);
            assert_eq!(bitmap.project_to_ui_segments(), [1.0; 16]);
            assert_eq!(bitmap.completion_fraction(), 1.0);
            assert!(bitmap
                .to_segment_snapshots(TransferStatus::Paused, 100)
                .iter()
                .all(|segment| segment.state == SegmentState::Completed && segment.speed_bps == 0));
        }
    }
}
