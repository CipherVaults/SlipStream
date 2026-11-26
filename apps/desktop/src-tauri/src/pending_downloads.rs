use slipstream_protocol::{AddHttpDownloadRequest, PendingDownloadPreview};
use std::{collections::HashMap, time::Duration};
use tokio::time::Instant;

const CAPACITY: usize = 32;
const TTL: Duration = Duration::from_secs(15 * 60);

pub(crate) struct PendingHttpEntry {
    pub request: AddHttpDownloadRequest,
    pub preview: PendingDownloadPreview,
}

impl Drop for PendingHttpEntry {
    fn drop(&mut self) {
        fn scrub(value: String) {
            let mut bytes = value.into_bytes();
            for byte in &mut bytes {
                // The original string allocation may contain credentials.
                unsafe { std::ptr::write_volatile(byte, 0) };
            }
        }
        for (name, value) in self.request.headers.drain() {
            scrub(name);
            scrub(value);
        }
        if let Some(referrer) = self.request.referrer.take() {
            scrub(referrer);
        }
        scrub(std::mem::take(&mut self.request.url));
    }
}

#[derive(Default)]
pub(crate) struct PendingDownloads {
    entries: HashMap<String, (Instant, PendingHttpEntry)>,
}

impl PendingDownloads {
    pub fn insert(&mut self, entry: PendingHttpEntry) -> Result<(), String> {
        if self.entries.len() >= CAPACITY {
            return Err("Too many download dialogs are open.".into());
        }
        self.entries.insert(
            entry.preview.pending_id.clone(),
            (Instant::now() + TTL, entry),
        );
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&PendingHttpEntry> {
        self.entries
            .get(id)
            .filter(|(deadline, _)| Instant::now() < *deadline)
            .map(|(_, entry)| entry)
    }

    pub fn take(&mut self, id: &str) -> Option<PendingHttpEntry> {
        self.get(id)?;
        self.entries.remove(id).map(|(_, entry)| entry)
    }

    pub fn discard(&mut self, id: &str) {
        self.entries.remove(id);
    }

    pub fn next_expiration(&self) -> Option<Instant> {
        self.entries.values().map(|(deadline, _)| *deadline).min()
    }

    pub fn expire(&mut self) -> Vec<String> {
        let now = Instant::now();
        self.entries
            .extract_if(|_, (deadline, _)| *deadline <= now)
            .map(|(id, _)| id)
            .collect()
    }

    pub fn clear(&mut self) -> Vec<String> {
        self.entries.drain().map(|(id, _)| id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> PendingHttpEntry {
        PendingHttpEntry {
            request: AddHttpDownloadRequest {
                url: "https://example.com/file?token=secret".into(),
                filename: None,
                save_dir: None,
                max_connections: None,
                queue_id: None,
                headers: HashMap::from([("Cookie".into(), "session=private".into())]),
                referrer: Some("https://example.com/source".into()),
                expected_sha256: None,
                expected_size_bytes: None,
                start_paused: false,
                overwrite_existing: false,
            },
            preview: PendingDownloadPreview {
                pending_id: id.into(),
                url: "https://example.com/file".into(),
                source_domain: "example.com".into(),
                filename: "file".into(),
                suggested_category_id: "general".into(),
                suggested_save_dir: String::new(),
                expected_size_bytes: None,
                duplicate: None,
            },
        }
    }

    #[test]
    fn pending_entries_are_bounded_expire_and_can_be_scrubbed() {
        let mut pending = PendingDownloads::default();
        for n in 0..CAPACITY {
            pending.insert(entry(&n.to_string())).unwrap();
        }
        assert!(pending.insert(entry("overflow")).is_err());
        let wire = serde_json::to_string(&pending.get("0").unwrap().preview).unwrap();
        for secret in ["headers", "Cookie", "private", "token=", "referrer"] {
            assert!(!wire.contains(secret));
        }
        pending.discard("0");
        assert!(pending.get("0").is_none());
        pending.insert(entry("after-cancel")).unwrap();
        pending.entries.get_mut("1").unwrap().0 = Instant::now();
        assert!(pending.get("1").is_none());
        assert!(pending.take("1").is_none());
        assert_eq!(pending.expire(), ["1"]);
        assert_eq!(pending.clear().len(), CAPACITY - 1);
        assert_eq!(pending.next_expiration(), None);
    }
}
