// src-tauri/src/provider_credentials/oauth/exchange.rs
//
// The code-for-key exchange (items.id=734): one POST of the approval code and
// the PKCE verifier to the provider's exchange address, which answers with an
// API key.
//
// WHERE IT GOES. The caller passes a URL that `url_policy::resolve_oauth`
// already accepted (https, the provider's own host). The production client is
// also `https_only` with redirects OFF, so even a URL that slipped through
// could not be plain http and a 3xx can never carry the code or verifier to
// another host.
//
// WHAT IT SENDS. JSON `{code, code_verifier, code_challenge_method}` and no
// Authorization header: the code and verifier are the whole credential.
//
// WHAT COMES BACK. `{ "key": "..." }`. The key is returned in a zeroizing
// string. Errors are stable codes; no error text carries any part of the
// request, the reply or the key. Nothing here logs.
//
// BEST-EFFORT HYGIENE. The request body and the reply bytes pass through the
// HTTP stack, which owns them; our own copies of the code, verifier and reply
// are in zeroizing buffers.

use std::time::Duration;

use reqwest::{redirect, Client, StatusCode};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::pkce::CHALLENGE_METHOD;
use crate::provider_credentials::key_pattern::normalise_key_text;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
/// Largest reply read. A key reply is a few hundred bytes.
const MAX_REPLY_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeError {
    /// The provider refused the code or verifier (expired, used, wrong).
    CodeRejected,
    RateLimited,
    /// No answer: could not connect, timed out.
    Unreachable,
    /// Any other failure status, including a redirect.
    Failed,
    /// An answer that was not a usable key reply.
    Malformed,
}

impl ExchangeError {
    pub fn code(self) -> &'static str {
        match self {
            Self::CodeRejected => "code_rejected",
            Self::RateLimited => "rate_limited",
            Self::Unreachable => "exchange_unreachable",
            Self::Failed => "exchange_failed",
            Self::Malformed => "exchange_malformed",
        }
    }
}

#[derive(Serialize)]
struct Request<'a> {
    code: &'a str,
    code_verifier: &'a str,
    code_challenge_method: &'static str,
}

#[derive(Deserialize)]
struct Reply {
    key: String,
}

pub struct ExchangeClient {
    http: Client,
    /// Test-only: the base (`http://127.0.0.1:port`) every request is sent to
    /// instead of the URL's own scheme and host, so a flow that resolved a
    /// real https address through the URL policy can still be pointed at a
    /// loopback fixture. The path is kept.
    #[cfg(test)]
    send_to: Option<String>,
    /// Test-only: the URLs `exchange` was asked to call (before any rewrite).
    #[cfg(test)]
    asked: std::sync::Mutex<Vec<String>>,
}

impl ExchangeClient {
    pub fn new() -> Self {
        let http = Client::builder()
            .https_only(true)
            .redirect(redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TOTAL_TIMEOUT)
            .build()
            .expect("failed to build the OAuth exchange HTTP client");
        Self {
            http,
            #[cfg(test)]
            send_to: None,
            #[cfg(test)]
            asked: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Test-only: plain-HTTP loopback client with proxies off and redirects
    /// off, so a loopback fixture can stand in for the provider. Does not
    /// exist in non-test builds.
    #[cfg(test)]
    pub(crate) fn for_loopback_tests() -> Self {
        let http = Client::builder()
            .no_proxy()
            .redirect(redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build the loopback test client");
        Self {
            http,
            send_to: None,
            asked: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Test-only: send every request to `base` (see `send_to`).
    #[cfg(test)]
    pub(crate) fn sending_to(mut self, base: &str) -> Self {
        self.send_to = Some(base.trim_end_matches('/').to_owned());
        self
    }

    /// Test-only: the URLs `exchange` was asked to call.
    #[cfg(test)]
    pub(crate) fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }

    pub async fn exchange(
        &self,
        url: &str,
        code: &str,
        verifier: &str,
    ) -> Result<Zeroizing<String>, ExchangeError> {
        let body = Zeroizing::new(
            serde_json::to_vec(&Request {
                code,
                code_verifier: verifier,
                code_challenge_method: CHALLENGE_METHOD,
            })
            .map_err(|_| ExchangeError::Failed)?,
        );
        #[cfg(test)]
        let url_owned = {
            self.asked.lock().unwrap().push(url.to_owned());
            match &self.send_to {
                Some(base) => {
                    let path = reqwest::Url::parse(url)
                        .map(|u| u.path().to_owned())
                        .unwrap_or_default();
                    format!("{base}{path}")
                }
                None => url.to_owned(),
            }
        };
        #[cfg(test)]
        let url = url_owned.as_str();
        let mut response = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|_| ExchangeError::Unreachable)?;

        let status = response.status();
        if !status.is_success() {
            return Err(match status {
                StatusCode::BAD_REQUEST
                | StatusCode::UNAUTHORIZED
                | StatusCode::FORBIDDEN
                | StatusCode::NOT_FOUND
                | StatusCode::GONE
                | StatusCode::UNPROCESSABLE_ENTITY => ExchangeError::CodeRejected,
                StatusCode::TOO_MANY_REQUESTS => ExchangeError::RateLimited,
                _ => ExchangeError::Failed,
            });
        }

        if response
            .content_length()
            .is_some_and(|n| n > MAX_REPLY_BYTES as u64)
        {
            return Err(ExchangeError::Malformed);
        }
        let mut buf = Zeroizing::new(Vec::<u8>::new());
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if buf.len() + chunk.len() > MAX_REPLY_BYTES {
                        return Err(ExchangeError::Malformed);
                    }
                    buf.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => return Err(ExchangeError::Unreachable),
            }
        }
        let reply: Reply = serde_json::from_slice(&buf).map_err(|_| ExchangeError::Malformed)?;
        let key = Zeroizing::new(reply.key);
        // The same syntax rule the save path applies, so a reply the save
        // would refuse never becomes a candidate.
        if normalise_key_text(&key).is_none_or(|k| k.len() != key.len()) {
            return Err(ExchangeError::Malformed);
        }
        Ok(key)
    }
}

impl Default for ExchangeClient {
    fn default() -> Self {
        Self::new()
    }
}
