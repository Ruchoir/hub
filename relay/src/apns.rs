//! Apple's push service (APNs), with the token-based provider authentication: a JWT signed (ES256)
//! with the team's `.p8` key, valid an hour, reused for forty minutes.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jwt_simple::prelude::{ECDSAP256KeyPairLike, ES256KeyPair};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::{exact_claims, Kind, Outcome, Pusher};

/// Production APNs: every Ruchoir build is signed for the App Store (TestFlight included).
pub const PRODUCTION_HOST: &str = "api.push.apple.com";
const TOKEN_REUSE: Duration = Duration::from_secs(40 * 60);

pub struct Apns {
    client: reqwest::Client,
    host: String,
    topic: String,
    team_id: String,
    key: ES256KeyPair,
    token: Mutex<Option<(String, Instant)>>,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    iat: u64,
}

/// What every push carries: an alert the phone's Notification Service Extension replaces with what
/// the instance says (`mutable-content`), and nothing else. Never any content (ADR 0006).
pub fn payload() -> Value {
    json!({
        "aps": {
            "alert": { "title": "Ruchoir", "body": "…" },
            "mutable-content": 1,
            "sound": "default"
        }
    })
}

/// What a push of this kind carries. A sync is a background push with nothing in it: iOS hands it
/// to the app, which counts its unread again; nothing is shown and no sound is played.
pub fn payload_for(kind: Kind) -> Value {
    match kind {
        Kind::Wake => payload(),
        Kind::Sync => json!({ "aps": { "content-available": 1 } }),
    }
}

/// The push type and priority Apple requires for each kind (a background push must be priority 5).
pub fn headers_for(kind: Kind) -> (&'static str, &'static str) {
    match kind {
        Kind::Wake => ("alert", "10"),
        Kind::Sync => ("background", "5"),
    }
}

/// What an APNs answer means for the device.
pub fn outcome(status: u16, reason: Option<&str>) -> Outcome {
    match (status, reason) {
        (200, _) => Outcome::Delivered,
        (410, _) => Outcome::Gone,
        (400, Some("BadDeviceToken" | "DeviceTokenNotForTopic")) => Outcome::Gone,
        (_, reason) => Outcome::Failed(format!("{status} {}", reason.unwrap_or(""))),
    }
}

impl Apns {
    pub fn new(
        pem: &str,
        key_id: &str,
        team_id: &str,
        topic: &str,
        host: &str,
    ) -> Result<Self, String> {
        let key = ES256KeyPair::from_pem(pem)
            .map_err(|e| format!("APNs key: {e}"))?
            .with_key_id(key_id);
        let client = reqwest::Client::builder()
            .http2_prior_knowledge()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("APNs client: {e}"))?;
        Ok(Self {
            client,
            host: host.to_owned(),
            topic: topic.to_owned(),
            team_id: team_id.to_owned(),
            key,
            token: Mutex::new(None),
        })
    }

    async fn bearer(&self, fresh: bool) -> Result<String, String> {
        let mut cached = self.token.lock().await;
        if let Some((token, made)) = cached.as_ref() {
            if !fresh && made.elapsed() < TOKEN_REUSE {
                return Ok(token.clone());
            }
        }
        let iat = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs();
        let token = self
            .key
            .sign(exact_claims(Claims {
                iss: &self.team_id,
                iat,
            }))
            .map_err(|e| format!("APNs token: {e}"))?;
        *cached = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    async fn send_once(&self, device: &str, kind: Kind, fresh: bool) -> (u16, Option<String>) {
        let bearer = match self.bearer(fresh).await {
            Ok(bearer) => bearer,
            Err(e) => return (0, Some(e)),
        };
        // A count that is an hour late is not worth correcting: a newer one will have come.
        let lifetime = match kind {
            Kind::Wake => 86_400,
            Kind::Sync => 3_600,
        };
        let expiration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() + lifetime)
            .unwrap_or(0);
        let (push_type, priority) = headers_for(kind);
        let response = self
            .client
            .post(format!("https://{}/3/device/{device}", self.host))
            .header("authorization", format!("bearer {bearer}"))
            .header("apns-topic", &self.topic)
            .header("apns-push-type", push_type)
            .header("apns-priority", priority)
            .header("apns-expiration", expiration.to_string())
            .json(&payload_for(kind))
            .send()
            .await;
        match response {
            Ok(response) => {
                let status = response.status().as_u16();
                let reason = response
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|v| v["reason"].as_str().map(str::to_owned));
                (status, reason)
            }
            Err(e) => (0, Some(e.to_string())),
        }
    }
}

impl Pusher for Apns {
    fn push<'a>(
        &'a self,
        token: &'a str,
        kind: Kind,
    ) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>> {
        Box::pin(async move {
            let (status, reason) = self.send_once(token, kind, false).await;
            // A provider token Apple no longer takes: make a new one and try once more.
            if status == 403 && reason.as_deref() == Some("ExpiredProviderToken") {
                let (status, reason) = self.send_once(token, kind, true).await;
                return outcome(status, reason.as_deref());
            }
            outcome(status, reason.as_deref())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_carries_nothing_but_a_placeholder() {
        let payload = payload();
        assert_eq!(payload["aps"]["mutable-content"], 1);
        assert_eq!(payload["aps"]["alert"]["title"], "Ruchoir");
        assert_eq!(
            payload.as_object().unwrap().len(),
            1,
            "nothing beside `aps`"
        );
    }

    #[test]
    fn a_sync_is_silent_and_carries_nothing() {
        let sync = payload_for(Kind::Sync);
        assert_eq!(sync, json!({ "aps": { "content-available": 1 } }));
        assert_eq!(payload_for(Kind::Wake), payload());
        assert_eq!(headers_for(Kind::Sync), ("background", "5"));
        assert_eq!(headers_for(Kind::Wake), ("alert", "10"));
    }

    #[test]
    fn apple_answers_are_read() {
        assert_eq!(outcome(200, None), Outcome::Delivered);
        assert_eq!(outcome(410, Some("Unregistered")), Outcome::Gone);
        assert_eq!(outcome(400, Some("BadDeviceToken")), Outcome::Gone);
        assert!(matches!(
            outcome(429, Some("TooManyRequests")),
            Outcome::Failed(_)
        ));
        assert!(matches!(outcome(0, Some("timeout")), Outcome::Failed(_)));
    }

    #[test]
    fn the_token_says_only_what_apple_asks() {
        let pem = jwt_simple::prelude::ES256KeyPair::generate()
            .to_pem()
            .unwrap();
        let apns = Apns::new(
            &pem,
            "KEY123",
            "TEAM456",
            "com.ruchoir.app",
            PRODUCTION_HOST,
        )
        .unwrap();
        let token = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(apns.bearer(true))
            .unwrap();
        let part = |i: usize| -> Value {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(token.split('.').nth(i).unwrap())
                .unwrap();
            serde_json::from_slice(&bytes).unwrap()
        };
        assert_eq!(part(0)["alg"], "ES256");
        assert_eq!(part(0)["kid"], "KEY123");
        let claims = part(1);
        let mut names: Vec<&str> = claims
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        assert_eq!(names, vec!["iat", "iss"]);
        assert_eq!(claims["iss"], "TEAM456");
    }

    #[test]
    fn a_key_that_is_not_one_is_refused() {
        assert!(Apns::new(
            "not a key",
            "KEY",
            "TEAM",
            "com.ruchoir.app",
            PRODUCTION_HOST
        )
        .is_err());
    }
}
