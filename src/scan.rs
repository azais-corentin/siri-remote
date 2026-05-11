//! BLE scanning, candidate ranking, and Apple-HID identity extraction.
//!
//! Ports the equivalent logic in `pair.py` (lock-on-pairing-mode-remote) and
//! `events.py` (single-pass settle scan) onto `btleplug`'s event stream.

use std::collections::HashMap;
use std::fmt::Write;
use std::time::Duration;

use btleplug::api::{Central, CentralEvent, Peripheral as _, PeripheralProperties, ScanFilter};
use btleplug::platform::{Adapter, PeripheralId};
use futures::StreamExt;
use tokio::time::{Instant, timeout};
use uuid::Uuid;

use crate::decoder::{APPLE_COMPANY_ID, APPLE_HID_MFR_PREFIX};

/// HID Service UUID (GATT short form `0x1812`, expanded to the BT base UUID).
pub const HID_SERVICE_UUID: Uuid = Uuid::from_u128(0x0000_1812_0000_1000_8000_0080_5f9b_34fb);

/// Minimum RSSI for a Siri Remote to be considered close enough to be the user's intended device.
pub const MIN_RSSI: i16 = -55;

/// Drop candidates whose last advertisement is older than this. Apple rotates
/// the BLE address every ~20s; once it rotates, the old address stops being
/// advertised and a connect attempt would time out.
pub const STALE_AFTER: Duration = Duration::from_secs(5);

/// Final selection from a scan, identified by Apple identity address.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub identity_address: String,
    pub peripheral_id: PeripheralId,
    pub last_address: String,
    pub last_rssi: i16,
    pub hits: usize,
    pub mean_rssi: f64,
    pub last_name: Option<String>,
}

struct Tracked {
    identity: String,
    rssis: Vec<i16>,
    last_id: PeripheralId,
    last_address: String,
    last_name: Option<String>,
    last_seen: Instant,
}

impl Tracked {
    fn mean(&self) -> f64 {
        if self.rssis.is_empty() {
            f64::NEG_INFINITY
        } else {
            self.rssis.iter().map(|r| *r as f64).sum::<f64>() / self.rssis.len() as f64
        }
    }
}

/// `True` if this advertisement matches a Siri Remote (in pairing mode):
/// HID service present, Apple manufacturer data with the HID prefix, and an
/// RSSI strong enough that the device must be the one the user is holding.
pub fn is_siri_remote(props: &PeripheralProperties) -> bool {
    let Some(rssi) = props.rssi else { return false };
    if rssi < MIN_RSSI {
        return false;
    }
    if !props.services.contains(&HID_SERVICE_UUID) {
        return false;
    }
    let Some(mfr) = props.manufacturer_data.get(&APPLE_COMPANY_ID) else {
        return false;
    };
    mfr.starts_with(&APPLE_HID_MFR_PREFIX)
}

/// Pull the Siri Remote's identity address out of the Apple manufacturer
/// payload. Observed layout in pairing mode:
///
/// ```text
/// 07 0d 02 15 03 02 | <6-byte identity address> | 4f 50 50
/// ```
///
/// Returns `None` if the buffer is too short or doesn't carry the HID prefix.
pub fn extract_identity_address(mfr: &[u8]) -> Option<String> {
    if mfr.len() < 12 || !mfr.starts_with(&APPLE_HID_MFR_PREFIX) {
        return None;
    }
    let mut s = String::with_capacity(17);
    for (i, b) in mfr[6..12].iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        let _ = write!(s, "{b:02X}");
    }
    Some(s)
}

/// Group advertisements by identity address. Used by both pair-style and
/// events-style scans; the consumer decides when to call [`Self::best_with`].
pub struct CandidateMap {
    inner: HashMap<String, Tracked>,
}

impl Default for CandidateMap {
    fn default() -> Self {
        Self::new()
    }
}

impl CandidateMap {
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    /// Inspect one `CentralEvent`; if it carries a Siri Remote advertisement,
    /// update the tracked candidate for that identity address.
    pub async fn ingest(&mut self, adapter: &Adapter, event: CentralEvent) {
        let id = match event {
            CentralEvent::DeviceDiscovered(id)
            | CentralEvent::DeviceUpdated(id)
            | CentralEvent::ManufacturerDataAdvertisement { id, .. }
            | CentralEvent::ServicesAdvertisement { id, .. } => id,
            _ => return,
        };

        let Ok(peripheral) = adapter.peripheral(&id).await else {
            return;
        };
        let Ok(Some(props)) = peripheral.properties().await else {
            return;
        };
        if !is_siri_remote(&props) {
            return;
        }
        let Some(mfr) = props.manufacturer_data.get(&APPLE_COMPANY_ID) else {
            return;
        };
        let Some(identity) = extract_identity_address(mfr) else {
            return;
        };

        let rssi = props.rssi.unwrap_or(i16::MIN);
        let address = props.address.to_string().to_uppercase();
        let name = props.local_name.clone();
        let now = Instant::now();

        let entry = self
            .inner
            .entry(identity.clone())
            .or_insert_with(|| Tracked {
                identity: identity.clone(),
                rssis: Vec::new(),
                last_id: id.clone(),
                last_address: address.clone(),
                last_name: name.clone(),
                last_seen: now,
            });
        entry.rssis.push(rssi);
        entry.last_id = id;
        entry.last_address = address;
        entry.last_name = name;
        entry.last_seen = now;

        let display_name = entry
            .last_name
            .clone()
            .unwrap_or_else(|| entry.identity.clone());
        eprintln!(
            "  identity={} addr={} name={:?} rssi={} hits={} mean_rssi={:.1}",
            entry.identity,
            entry.last_address,
            display_name,
            rssi,
            entry.rssis.len(),
            entry.mean(),
        );
    }

    /// Return the strongest fresh candidate with `>= min_hits` advertisements.
    /// Emits the same "Locked on identity …" line that `pair.py` prints when a
    /// pick is made, plus the "other Siri Remote(s) also in range" tail.
    pub fn best_with(&self, window: Duration, min_hits: usize) -> Option<Candidate> {
        let now = Instant::now();
        let mut fresh: Vec<&Tracked> = self
            .inner
            .values()
            .filter(|t| {
                t.rssis.len() >= min_hits && now.saturating_duration_since(t.last_seen) <= window
            })
            .collect();
        fresh.sort_by(|a, b| {
            b.mean()
                .partial_cmp(&a.mean())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let best = fresh.first()?;
        let last_rssi = *best.rssis.last()?;
        let cand = Candidate {
            identity_address: best.identity.clone(),
            peripheral_id: best.last_id.clone(),
            last_address: best.last_address.clone(),
            last_rssi,
            hits: best.rssis.len(),
            mean_rssi: best.mean(),
            last_name: best.last_name.clone(),
        };
        eprintln!(
            "\nLocked on identity {} via current address {} (mean RSSI {:.1} over {} adverts, last rssi {})",
            cand.identity_address, cand.last_address, cand.mean_rssi, cand.hits, cand.last_rssi,
        );
        if fresh.len() > 1 {
            eprintln!(
                "  ({} other Siri Remote(s) also in range; picked the one with the strongest signal)",
                fresh.len() - 1,
            );
        }
        Some(cand)
    }
}

/// Outcome bands for [`scan_for_remote`]: either we locked on, or the overall
/// timeout elapsed without a confident pick, or some lower-level error.
pub enum ScanError {
    Timeout,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for ScanError {
    fn from(e: anyhow::Error) -> Self {
        ScanError::Other(e)
    }
}

/// Pair-style scan loop: settle for `settle`, then re-rank every couple of
/// seconds; lock in when a candidate with `>= 2` hits stays fresh. Bounded by
/// `overall_timeout` end-to-end.
pub async fn scan_for_remote(
    adapter: &Adapter,
    settle: Duration,
    overall_timeout: Duration,
) -> Result<Candidate, ScanError> {
    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(|e| ScanError::Other(anyhow::anyhow!(e.to_string())))?;
    let mut events = adapter
        .events()
        .await
        .map_err(|e| ScanError::Other(anyhow::anyhow!(e.to_string())))?;

    eprintln!(
        "Scanning for {:.0}s for a Siri Remote in pairing mode...",
        settle.as_secs_f64()
    );

    let start = Instant::now();
    let settle_deadline = start + settle;
    let mut map = CandidateMap::new();
    let mut settle_announced = false;

    let result = loop {
        let now = Instant::now();
        if now.saturating_duration_since(start) > overall_timeout {
            break Err(ScanError::Timeout);
        }
        if now >= settle_deadline
            && let Some(best) = map.best_with(STALE_AFTER, 2)
        {
            break Ok(best);
        }

        let pause = if now < settle_deadline {
            (settle_deadline - now).min(Duration::from_secs(2))
        } else {
            Duration::from_secs(2)
        };
        match timeout(pause, events.next()).await {
            Ok(Some(ev)) => map.ingest(adapter, ev).await,
            Ok(None) => {
                break Err(ScanError::Other(anyhow::anyhow!(
                    "BLE event stream ended unexpectedly"
                )));
            }
            Err(_) => {
                if Instant::now() >= settle_deadline && !settle_announced {
                    eprintln!("  no fresh qualifying candidate yet; scanning 2s more...");
                    settle_announced = true;
                }
            }
        }
    };

    let _ = adapter.stop_scan().await;
    result
}

/// Events-style scan: drain advertisements for `settle` duration, then return
/// the strongest fresh candidate (no minimum hit requirement). Used to discover
/// the nearest currently advertising Siri Remote — could be bonded-and-quiet or
/// in pairing mode.
pub async fn scan_for_nearest_remote(adapter: &Adapter, settle: Duration) -> Option<Candidate> {
    adapter.start_scan(ScanFilter::default()).await.ok()?;
    let mut events = adapter.events().await.ok()?;

    eprintln!(
        "Scanning {:.0}s for Siri Remote advertisements...",
        settle.as_secs_f64()
    );
    let mut map = CandidateMap::new();
    let deadline = Instant::now() + settle;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, events.next()).await {
            Ok(Some(ev)) => map.ingest(adapter, ev).await,
            _ => break,
        }
    }
    let _ = adapter.stop_scan().await;

    // Python loosens the freshness window to `max(STALE_AFTER, settle + 1.0)`
    // so that adverts captured at the start of the scan still qualify.
    let window = STALE_AFTER.max(settle + Duration::from_secs(1));
    map.best_with(window, 1)
}

// -- Tests --------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_identity_address_known_payload() {
        // Documented example from pair.py + README: identity bytes at offset 6..12
        // are `10 b9 c4 01 a3 c0` so the formatted address is `10:B9:C4:01:A3:C0`.
        let mfr = [
            0x07, 0x0d, 0x02, 0x15, 0x03, 0x02, 0x10, 0xb9, 0xc4, 0x01, 0xa3, 0xc0, 0x4f, 0x50,
            0x50,
        ];
        assert_eq!(
            extract_identity_address(&mfr),
            Some("10:B9:C4:01:A3:C0".to_string())
        );
    }

    #[test]
    fn extract_identity_address_rejects_short_payload() {
        assert!(extract_identity_address(&[]).is_none());
        assert!(extract_identity_address(&[0x07, 0x0d]).is_none());
        // 11 bytes total — one short of the slice end (offset 12).
        let short = [
            0x07, 0x0d, 0x02, 0x15, 0x03, 0x02, 0x10, 0xb9, 0xc4, 0x01, 0xa3,
        ];
        assert!(extract_identity_address(&short).is_none());
    }

    #[test]
    fn extract_identity_address_rejects_wrong_prefix() {
        // First two bytes are not `07 0D` -> not the Apple HID advertisement.
        let mfr = [
            0xff, 0x00, 0x02, 0x15, 0x03, 0x02, 0x10, 0xb9, 0xc4, 0x01, 0xa3, 0xc0,
        ];
        assert!(extract_identity_address(&mfr).is_none());
    }
}
