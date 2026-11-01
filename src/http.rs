use std::time::Duration;

use rand::Rng;
use wreq::Client;
use wreq_util::{Emulation, Platform, Profile};

/// How many times to try a single URL before giving up.
const MAX_ATTEMPTS: u32 = 4;

/// Chrome profile used for TLS, HTTP/2, and header emulation.
///
/// Royal Road sits behind Cloudflare. On 2026-11-01 a reqwest client and curl
/// both received HTTP 403 with `cf-mitigated: challenge` and a "Just a moment"
/// Turnstile page, including when the User-Agent and Accept headers were copied
/// from Chrome. The rejection happens before the page is served: Cloudflare
/// scores the TLS/HTTP2 fingerprint, then on some networks (including this
/// datacenter) shows an interactive checkbox even to a real browser. Matching
/// a current Chrome fingerprint is what lets a non-browser client through when
/// Cloudflare is willing to skip the checkbox. The emulated profile supplies
/// its own User-Agent; overriding it makes the fingerprint inconsistent.
fn chrome_emulation() -> Emulation {
    Emulation::builder()
        .profile(Profile::Chrome149)
        .platform(Platform::Windows)
        .build()
}

/// Browser-impersonating HTTP client shared by metadata, chapter, and image fetches.
#[derive(Clone)]
pub struct HttpClient {
    inner: Client,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HttpClient(chrome149)")
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new().expect("failed to build the Royal Road HTTP client")
    }
}

impl HttpClient {
    pub fn new() -> eyre::Result<Self> {
        let inner = Client::builder()
            .emulation(chrome_emulation())
            .cookie_store(true)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(45))
            .build()
            .map_err(|err| eyre::eyre!("failed to build HTTP client: {err}"))?;
        Ok(Self { inner })
    }

    /// Fetch a page that should be HTML. Retries transient failures and
    /// Cloudflare challenge responses. Errors if the final body is still a challenge.
    pub async fn get_html(&self, url: &str, referer: Option<&str>) -> eyre::Result<String> {
        let bytes = self.get_with_retries(url, referer).await?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if is_cloudflare_challenge(&text) {
            eyre::bail!(
                "Royal Road returned a Cloudflare check for {url} after {MAX_ATTEMPTS} attempts. \
                 The client impersonates Chrome's TLS fingerprint, which is what gets through \
                 when Cloudflare does not demand the interactive Turnstile checkbox. This network \
                 still served that checkbox; retry later from a regular connection."
            );
        }
        Ok(text)
    }

    /// Fetch bytes (cover and inline images). Same retry policy as [`Self::get_html`].
    pub async fn get_bytes(&self, url: &str, referer: Option<&str>) -> eyre::Result<Vec<u8>> {
        let bytes = self.get_with_retries(url, referer).await?;
        if is_cloudflare_challenge(&String::from_utf8_lossy(&bytes)) {
            eyre::bail!("Cloudflare challenge while downloading {url}");
        }
        Ok(bytes)
    }
}

impl HttpClient {
    async fn get_with_retries(&self, url: &str, referer: Option<&str>) -> eyre::Result<Vec<u8>> {
        let mut last_error: Option<eyre::Report> = None;
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                let delay = retry_backoff(attempt);
                tracing::warn!(
                    "Retrying {url} in {:.1}s (attempt {}/{MAX_ATTEMPTS})",
                    delay.as_secs_f32(),
                    attempt + 1
                );
                tokio::time::sleep(delay).await;
            }

            match self.fetch_once(url, referer).await {
                Ok((status, challenge, bytes)) => {
                    let body_challenge = is_cloudflare_challenge(&String::from_utf8_lossy(&bytes));
                    let retryable = challenge
                        || body_challenge
                        || matches!(status, 403 | 408 | 425 | 429 | 500 | 502 | 503 | 504);
                    if retryable {
                        let why = if challenge || body_challenge {
                            "Cloudflare challenge"
                        } else {
                            "transient HTTP status"
                        };
                        tracing::warn!("{why} (HTTP {status}) for {url}");
                        last_error = Some(eyre::eyre!("HTTP {status} from {url} ({why})"));
                        continue;
                    }
                    if !(200..300).contains(&status) {
                        return Err(eyre::eyre!("HTTP {status} from {url}"));
                    }
                    return Ok(bytes);
                }
                Err(err) => {
                    tracing::warn!("Request error for {url}: {err}");
                    last_error = Some(err);
                }
            }
        }

        let err = last_error.unwrap_or_else(|| eyre::eyre!("request failed for {url}"));
        let message = format!("{err:#}");
        if message.contains("Cloudflare") {
            Err(eyre::eyre!(
                "{message}. Royal Road is behind Cloudflare, which rejected this client with an \
                 interactive Turnstile check after {MAX_ATTEMPTS} attempts. The downloader \
                 impersonates Chrome's TLS fingerprint, which is what gets past the block that \
                 reqwest and curl hit. If this network still demands the checkbox, retry later \
                 from a regular connection."
            ))
        } else {
            Err(err)
        }
    }

    async fn fetch_once(
        &self,
        url: &str,
        referer: Option<&str>,
    ) -> eyre::Result<(u16, bool, Vec<u8>)> {
        let mut request = self.inner.get(url);
        if let Some(referer) = referer {
            request = request.header("referer", referer);
        }
        let response = request
            .send()
            .await
            .map_err(|err| eyre::eyre!("request to {url} failed: {err}"))?;
        let status = response.status().as_u16();
        let challenge = response
            .headers()
            .get("cf-mitigated")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("challenge"));
        let bytes = response
            .bytes()
            .await
            .map_err(|err| eyre::eyre!("reading {url} failed: {err}"))?
            .to_vec();
        Ok((status, challenge, bytes))
    }
}

/// Pause between chapter downloads so a long run stays polite.
pub async fn polite_delay() {
    let jitter = rand::thread_rng().gen_range(0..400u64);
    tokio::time::sleep(Duration::from_millis(800 + jitter)).await;
}

fn retry_backoff(attempt: u32) -> Duration {
    let base_ms = 1000u64.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1)));
    let jitter = rand::thread_rng().gen_range(0..500u64);
    Duration::from_millis(base_ms + jitter)
}

pub fn is_cloudflare_challenge(body: &str) -> bool {
    let head: String = body.chars().take(2500).collect();
    let head = head.to_ascii_lowercase();
    head.contains("<title>just a moment")
        || head.contains("performing security verification")
        || head.contains("cf-challenge")
        || (head.contains("challenges.cloudflare.com") && head.contains("just a moment"))
}

#[cfg(test)]
mod tests {
    use super::is_cloudflare_challenge;

    #[test]
    fn detects_turnstile_interstitial() {
        let html = "<html><head><title>Just a moment...</title></head><body>Performing security verification</body></html>";
        assert!(is_cloudflare_challenge(html));
    }

    #[test]
    fn ignores_a_normal_fiction_page() {
        let html = "<html><head><title>The Runesmith | Royal Road</title></head><body><h1>The Runesmith</h1></body></html>";
        assert!(!is_cloudflare_challenge(html));
    }
}
