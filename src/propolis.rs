//! Propolis, as the hub calls it in its own name: its change feed (the members' replica) and its
//! mail (the unread digest), with service tokens asked by `private_key_jwt` and the client key the
//! installer generated.

use std::sync::Arc;

use serde::Deserialize;

/// The hub's client at Propolis.
pub struct Propolis {
    issuer: String,
    credentials: ruchoir_auth::Credentials,
    http: reqwest::Client,
}

impl Propolis {
    pub fn new(
        issuer: &str,
        credentials: ruchoir_auth::Credentials,
        http: reqwest::Client,
    ) -> Arc<Self> {
        Arc::new(Self {
            issuer: issuer.trim_end_matches('/').to_owned(),
            credentials,
            http,
        })
    }

    /// Propolis's address, without a trailing slash.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The HTTP client to reach it with.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// A service token carrying `scope`, in the hub's own name.
    ///
    /// Asked here rather than through `ruchoir_auth::Client`, whose futures are not `Send` in core
    /// v0.2.0 (fixed for the next release): this runs in spawned tasks.
    pub async fn token(&self, scope: &str) -> Result<String, String> {
        #[derive(Deserialize)]
        struct Answer {
            access_token: String,
        }
        let assertion = self
            .credentials
            .assertion(&self.issuer)
            .map_err(|e| e.to_string())?;
        let body = {
            let mut form = form_urlencoded::Serializer::new(String::new());
            form.append_pair("grant_type", "client_credentials")
                .append_pair("scope", scope)
                .append_pair("client_assertion_type", ruchoir_auth::client::JWT_BEARER)
                .append_pair("client_assertion", &assertion);
            form.finish()
        };
        let answer: Answer = self
            .http
            .post(format!("{}/oauth/token", self.issuer))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?
            .error_for_status()
            .map_err(|e| e.without_url().to_string())?
            .json()
            .await
            .map_err(|e| e.without_url().to_string())?;
        Ok(answer.access_token)
    }
}
