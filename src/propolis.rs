//! Propolis, as the hub calls it in its own name: its change feed (the members' replica) and its
//! mail (the unread digest), with service tokens asked by `private_key_jwt` and the client key the
//! installer generated.

use std::sync::Arc;

/// The hub's client at Propolis.
pub struct Propolis {
    issuer: String,
    client: ruchoir_auth::Client,
    http: reqwest::Client,
}

impl Propolis {
    pub fn new(
        issuer: &str,
        credentials: ruchoir_auth::Credentials,
        http: reqwest::Client,
    ) -> Arc<Self> {
        let issuer = issuer.trim_end_matches('/').to_owned();
        Arc::new(Self {
            client: ruchoir_auth::Client::new(&issuer, credentials, http.clone()),
            issuer,
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

    /// A service token carrying `scope`, in the hub's own name, kept by `ruchoir-auth` until a
    /// minute before it expires.
    pub async fn token(&self, scope: &str) -> Result<String, String> {
        self.client
            .client_credentials(None, &[scope])
            .await
            .map(|tokens| tokens.access_token)
            .map_err(|e| e.to_string())
    }
}
