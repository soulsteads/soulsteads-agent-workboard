//! Outbound webhook delivery.
//!
//! Each webhook keeps a persisted cursor into the event log, so delivery is ordered
//! per webhook, survives restarts, and is at-least-once: a receiver must tolerate
//! a repeat of the event id it sees in `X-Workboard-Event-Id`. A failing endpoint
//! is retried with exponential backoff and paused after repeated failures, which
//! never delays other webhooks. Lease tokens are always removed from payloads.
use crate::{Event, Store, redact_leases, store::WebhookTarget};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinSet};

/// Consecutive failures before a webhook is paused (about four hours of outage).
pub const MAX_FAILURES: u32 = 25;
const BATCH: usize = 25;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
/// Writes from other processes cannot signal this one, so the log is also polled.
const POLL: Duration = Duration::from_secs(1);

pub type Database = Arc<Mutex<Store>>;

/// `sha256=<hex>` over `"{timestamp}.{body}"`, keyed by the webhook secret.
pub fn sign(secret: &str, timestamp: i64, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(format!("{timestamp}.{body}").as_bytes());
    format!("sha256={:x}", mac.finalize().into_bytes())
}
fn payload(event: &Event) -> String {
    let mut value = json!({
        "id": event.id,
        "action": event.action,
        "issue": event.issue,
        "actor": event.actor,
        "created_at": event.created_at,
        "data": event.data,
    });
    redact_leases(&mut value);
    value.to_string()
}
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into()
}
/// Error text is stored and shown to operators, so it never includes the URL.
fn describe(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Timeout(_) => "request timed out".into(),
        ureq::Error::HostNotFound => "host not found".into(),
        ureq::Error::Io(e) => format!("connection error: {}", e.kind()),
        _ => "request failed".into(),
    }
}
fn send(agent: &ureq::Agent, target: &WebhookTarget, event: &Event) -> Result<(), String> {
    let body = payload(event);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let response = agent
        .post(&target.webhook.url)
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            concat!("workboard/", env!("CARGO_PKG_VERSION")),
        )
        .header("X-Workboard-Event", event.action.as_str())
        .header("X-Workboard-Event-Id", event.id.to_string().as_str())
        .header("X-Workboard-Webhook", target.webhook.id.as_str())
        .header("X-Workboard-Timestamp", timestamp.to_string().as_str())
        .header(
            "X-Workboard-Signature",
            sign(&target.secret, timestamp, &body).as_str(),
        )
        .send(body.as_str())
        .map_err(|e| describe(&e))?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("endpoint returned HTTP {}", status.as_u16()))
    }
}

enum Outcome {
    /// Everything available was delivered, or there was nothing to deliver.
    Settled,
    Failed {
        id: String,
        failures: u32,
    },
}

/// Delivers one webhook's pending events in order, stopping at the first failure.
fn deliver(db: &Database, agent: &ureq::Agent, target: WebhookTarget) -> Outcome {
    let id = target.webhook.id.clone();
    let lock = || db.lock().map_err(|_| "store unavailable".to_string());
    let batch = lock().and_then(|s| {
        s.webhook_batch(&target.webhook, BATCH)
            .map_err(|e| e.to_string())
    });
    let (events, scan_end) = match batch {
        Ok(batch) => batch,
        Err(_) => return Outcome::Settled,
    };
    for event in &events {
        match send(agent, &target, event) {
            Ok(()) => {
                if lock()
                    .and_then(|mut s| {
                        s.webhook_ack(&id, event.id, true)
                            .map_err(|e| e.to_string())
                    })
                    .is_err()
                {
                    return Outcome::Settled;
                }
            }
            Err(message) => {
                let failures = lock()
                    .and_then(|mut s| {
                        s.webhook_fail(&id, &message, MAX_FAILURES)
                            .map_err(|e| e.to_string())
                    })
                    .unwrap_or(1);
                return Outcome::Failed { id, failures };
            }
        }
    }
    if scan_end > target.webhook.cursor {
        let _ = lock().and_then(|mut s| {
            s.webhook_ack(&id, scan_end, false)
                .map_err(|e| e.to_string())
        });
    }
    Outcome::Settled
}
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.min(20)).min(MAX_BACKOFF)
}

/// Runs until the task is dropped. `wake` is signalled after in-process writes so
/// deliveries start immediately instead of at the next poll.
pub async fn run(db: Database, mut wake: watch::Receiver<u64>) {
    let agent = agent();
    let mut retry_at: HashMap<String, Instant> = HashMap::new();
    loop {
        let targets = {
            let db = db.clone();
            tokio::task::spawn_blocking(move || {
                db.lock().ok().and_then(|s| s.webhook_targets().ok())
            })
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
        };
        retry_at.retain(|id, _| targets.iter().any(|t| &t.webhook.id == id));
        let mut tasks = JoinSet::new();
        for target in targets {
            if retry_at
                .get(&target.webhook.id)
                .is_some_and(|until| *until > Instant::now())
            {
                continue;
            }
            retry_at.remove(&target.webhook.id);
            let (db, agent) = (db.clone(), agent.clone());
            tasks.spawn_blocking(move || deliver(&db, &agent, target));
        }
        while let Some(done) = tasks.join_next().await {
            if let Ok(Outcome::Failed { id, failures }) = done {
                retry_at.insert(id, Instant::now() + backoff(failures));
            }
        }
        tokio::select! {
            changed = wake.changed() => {
                if changed.is_err() {
                    tokio::time::sleep(POLL).await;
                }
            }
            _ = tokio::time::sleep(POLL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_an_independently_computed_hmac_sha256() {
        // Reference value from Python: hmac.new(b"whsec_test", b'1700000000.{"id":1}', sha256)
        assert_eq!(
            sign("whsec_test", 1_700_000_000, r#"{"id":1}"#),
            "sha256=2f441ba4b3b2d50d28a9ab9d9fd8880376ecd1eb5d0435401553f5d8d0a5dcf8"
        );
        assert_ne!(
            sign("whsec_test", 1_700_000_001, r#"{"id":1}"#),
            sign("whsec_test", 1_700_000_000, r#"{"id":1}"#)
        );
    }
    #[test]
    fn backoff_doubles_and_is_capped() {
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(4), Duration::from_secs(16));
        assert_eq!(backoff(25), MAX_BACKOFF);
    }
}
