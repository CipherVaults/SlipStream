use slipstream_protocol::{AddHttpDownloadRequest, PendingDownloadPreview};
use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

const CAPACITY: usize = 32;
const TTL: Duration = Duration::from_secs(15 * 60);
pub(crate) const PENDING_UNAVAILABLE: &str = "Pending download expired or was closed.";

pub(crate) struct PendingHttpEntry {
    pub request: AddHttpDownloadRequest,
    pub preview: PendingDownloadPreview,
}

impl Drop for PendingHttpEntry {
    fn drop(&mut self) {
        scrub_http_request(&mut self.request);
    }
}

pub(crate) struct ScrubbedRequest(pub Option<AddHttpDownloadRequest>);

impl Drop for ScrubbedRequest {
    fn drop(&mut self) {
        if let Some(request) = &mut self.0 {
            scrub_http_request(request);
        }
    }
}

fn scrub_http_request(request: &mut AddHttpDownloadRequest) {
    fn scrub(value: String) {
        let mut bytes = value.into_bytes();
        for byte in &mut bytes {
            // The original string allocation may contain credentials.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
    for (name, value) in request.headers.drain() {
        scrub(name);
        scrub(value);
    }
    if let Some(referrer) = request.referrer.take() {
        scrub(referrer);
    }
    scrub(std::mem::take(&mut request.url));
}

#[derive(Default)]
pub(crate) struct PendingDownloads {
    entries: HashMap<String, (Instant, PendingHttpEntry)>,
    confirming: HashSet<String>,
}

pub(crate) struct PendingConfirmation<'a> {
    pending: &'a Mutex<PendingDownloads>,
    changed: &'a Notify,
    id: &'a str,
}

impl<'a> PendingConfirmation<'a> {
    pub fn begin(
        pending: &'a Mutex<PendingDownloads>,
        changed: &'a Notify,
        id: &'a str,
    ) -> Result<Self, String> {
        let mut downloads = pending.lock().unwrap();
        downloads.get(id).ok_or(PENDING_UNAVAILABLE)?;
        if !downloads.confirming.insert(id.to_owned()) {
            return Err("This download is already being confirmed.".into());
        }
        Ok(Self {
            pending,
            changed,
            id,
        })
    }
}

impl Drop for PendingConfirmation<'_> {
    fn drop(&mut self) {
        self.pending.lock().unwrap().confirming.remove(self.id);
        self.changed.notify_one();
    }
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
            .filter(|(deadline, _)| Instant::now() < *deadline || self.confirming.contains(id))
            .map(|(_, entry)| entry)
    }

    pub fn take(&mut self, id: &str) -> Option<PendingHttpEntry> {
        self.get(id)?;
        self.entries.remove(id).map(|(_, entry)| entry)
    }

    pub fn discard(&mut self, id: &str) -> bool {
        if self.confirming.contains(id) {
            return false;
        }
        self.entries.remove(id);
        true
    }

    pub fn next_expiration(&self) -> Option<Instant> {
        self.entries
            .iter()
            .filter(|(id, _)| !self.confirming.contains(*id))
            .map(|(_, (deadline, _))| *deadline)
            .min()
    }

    pub fn expire(&mut self) -> Vec<String> {
        let now = Instant::now();
        self.entries
            .extract_if(|id, (deadline, _)| *deadline <= now && !self.confirming.contains(id))
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

    #[test]
    fn in_flight_confirmation_keeps_the_pending_entry_and_rejects_another() {
        let pending = Mutex::new(PendingDownloads::default());
        let changed = Notify::new();
        pending.lock().unwrap().insert(entry("confirm")).unwrap();
        let confirming = PendingConfirmation::begin(&pending, &changed, "confirm").unwrap();
        assert!(PendingConfirmation::begin(&pending, &changed, "confirm").is_err());
        assert!(!pending.lock().unwrap().discard("confirm"));
        assert!(pending.lock().unwrap().get("confirm").is_some());
        drop(confirming);
        let retry = PendingConfirmation::begin(&pending, &changed, "confirm").unwrap();
        assert!(pending.lock().unwrap().take("confirm").is_some());
        drop(retry);
        assert!(PendingConfirmation::begin(&pending, &changed, "confirm").is_err());
    }
}
