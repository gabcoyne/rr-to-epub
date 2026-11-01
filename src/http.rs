//! HTTP fetches for Royal Road.
//!
//! Cloudflare in front of royalroad.com rejects this program's old reqwest/rustls
//! client with HTTP 403 (`cf-mitigated: challenge`), and a browser User-Agent does
//! not change that. BoringSSL Chrome profiles through Chrome 149 (wreq) were also
//! challenged on 2026-11-01. Chrome 152 impersonation, as implemented by
//! curl-impersonate, receives the real page, but individual requests are still
//! challenged intermittently, so each fetch is retried with backoff.
//!
//! The curl-impersonate binary is taken from `RR_CURL_IMPERSONATE`, from `PATH`,
//! or downloaded once into the cache directory.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use rand::Rng;

const RELEASE: &str = "v2.3.2";
const IMPERSONATE: &str = "chrome152";

struct ReleaseAsset {
    name: &'static str,
    sha256: &'static str,
}

fn bundled_asset() -> Option<ReleaseAsset> {
    if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "gnu"
    )) {
        Some(ReleaseAsset {
            name: "curl-impersonate-v2.3.2.x86_64-linux-gnu.tar.gz",
            sha256: "bc9bd2522fe1850dd54c0fff189c8dd606df3f222ab2a905f2dd255123c3f495",
        })
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        Some(ReleaseAsset {
            name: "curl-impersonate-v2.3.2.x86_64-linux-musl.tar.gz",
            sha256: "ce79849f3be2e15d06364f89aeebb901b6be7b48947e2af8dae0616bbc87731f",
        })
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "aarch64",
        target_env = "gnu"
    )) {
        Some(ReleaseAsset {
            name: "curl-impersonate-v2.3.2.aarch64-linux-gnu.tar.gz",
            sha256: "35a351db883f5ce82945bd8280c727b08e27aa48594d4eb9e091d8c80e936d52",
        })
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "aarch64",
        target_env = "musl"
    )) {
        Some(ReleaseAsset {
            name: "curl-impersonate-v2.3.2.aarch64-linux-musl.tar.gz",
            sha256: "1519bcee11f9c93a7ed95026bd951d53e8c7cd093f3ac53651b143e5a1e2098f",
        })
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(ReleaseAsset {
            name: "curl-impersonate-v2.3.2.arm64-macos.tar.gz",
            sha256: "638ed43cc93cb5da91f794ff23fce1735fec3dc5c09ee87cc499d802c25d2d35",
        })
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some(ReleaseAsset {
            name: "curl-impersonate-v2.3.2.x86_64-macos.tar.gz",
            sha256: "4e9fe855a34d011de88bf216c196f6957624526678b99b4f2135acc9ffa8c156",
        })
    } else {
        None
    }
}

pub struct HttpClient {
    cookie_jar: PathBuf,
}

impl HttpClient {
    pub fn new() -> eyre::Result<Self> {
        let cache = crate::cache::Cache::cache_path()?;
        let cookie_jar = cache.join("cookies.txt");
        if !cookie_jar.exists() {
            std::fs::write(&cookie_jar, [])?;
        }
        Ok(Self { cookie_jar })
    }

    pub async fn get_text(&self, url: &str) -> eyre::Result<String> {
        let bytes = self.request(url, 12, true).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub async fn get_bytes(&self, url: &str) -> eyre::Result<Vec<u8>> {
        self.request(url, 4, false).await
    }

    async fn request(
        &self,
        url: &str,
        max_attempts: u32,
        retry_plain_403: bool,
    ) -> eyre::Result<Vec<u8>> {
        let mut last_status = 0u16;
        for attempt in 1..=max_attempts {
            match self.send_once(url) {
                Ok((status, mitigated, bytes)) => {
                    last_status = status;
                    let sample = sample_text(&bytes);
                    let challenge =
                        is_cloudflare_challenge(status, mitigated, bytes.len(), &sample);
                    if (200..300).contains(&status) && !challenge {
                        return Ok(bytes);
                    }
                    let retry = challenge
                        || (retry_plain_403 && status == 403)
                        || matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504);
                    if !retry || attempt == max_attempts {
                        let why = if challenge {
                            " (Cloudflare challenge)"
                        } else {
                            ""
                        };
                        return Err(eyre::eyre!("request failed ({status}) for {url}{why}"));
                    }
                    let wait = backoff(attempt);
                    tracing::warn!(
                        "Request to {url} returned {status}{}; retrying in {:.1}s ({attempt}/{max_attempts})",
                        if challenge {
                            " (Cloudflare challenge)"
                        } else {
                            ""
                        },
                        wait.as_secs_f32(),
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(err) => {
                    if attempt == max_attempts {
                        return Err(err);
                    }
                    let wait = backoff(attempt);
                    tracing::warn!(
                        "Request to {url} failed: {err:#}; retrying in {:.1}s ({attempt}/{max_attempts})",
                        wait.as_secs_f32()
                    );
                    tokio::time::sleep(wait).await;
                }
            }
        }
        Err(eyre::eyre!("request failed ({last_status}) for {url}"))
    }

    fn send_once(&self, url: &str) -> eyre::Result<(u16, bool, Vec<u8>)> {
        // Royal Road HTML is brotli/gzip. Some cover objects are labeled
        // `content-encoding: aws-chunked`, which `--compressed` refuses to decode.
        match self.curl_request(url, true) {
            Err(err) if is_bad_content_encoding(&err) => self.curl_request(url, false),
            other => other,
        }
    }

    fn curl_request(&self, url: &str, compressed: bool) -> eyre::Result<(u16, bool, Vec<u8>)> {
        let dir = tempfile::tempdir()?;
        let body_path = dir.path().join("body");
        let header_path = dir.path().join("headers");
        let curl = ensure_curl_impersonate()?;
        let mut command = Command::new(&curl);
        command.arg("--impersonate").arg(IMPERSONATE).arg("-L");
        if compressed {
            command.arg("--compressed");
        }
        let output = command
            .arg("--max-redirs")
            .arg("10")
            .arg("-sS")
            .arg("--max-time")
            .arg("60")
            .arg("--cookie")
            .arg(&self.cookie_jar)
            .arg("--cookie-jar")
            .arg(&self.cookie_jar)
            .arg("-D")
            .arg(&header_path)
            .arg("-o")
            .arg(&body_path)
            .arg("-w")
            .arg("%{http_code}")
            .arg(url)
            .output()
            .map_err(|err| {
                eyre::eyre!(
                    "failed to run curl-impersonate at {}: {err}",
                    curl.display()
                )
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eyre::bail!(
                "curl-impersonate exited with {}: {}",
                output.status,
                stderr.trim()
            );
        }
        let status_text = String::from_utf8_lossy(&output.stdout);
        let status: u16 = status_text
            .trim()
            .parse()
            .map_err(|_| eyre::eyre!("curl-impersonate returned no HTTP status for {url}"))?;
        let headers = std::fs::read_to_string(&header_path).unwrap_or_default();
        let mitigated = headers.lines().any(|line| {
            let lower = line.to_ascii_lowercase();
            lower.starts_with("cf-mitigated:")
        });
        let bytes = std::fs::read(&body_path)?;
        Ok((status, mitigated, bytes))
    }
}

fn is_bad_content_encoding(err: &eyre::Report) -> bool {
    let text = format!("{err:#}");
    text.contains("Unrecognized content encoding") || text.contains("curl: (61)")
}

fn backoff(attempt: u32) -> Duration {
    // Challenges are intermittent, not a hard rate limit. Keep the gap short
    // enough that a bad streak still finishes, and leave the longer pause
    // between successful chapter downloads to stay polite.
    let base = match attempt {
        1 => 800,
        2 => 1_200,
        3 => 1_800,
        4 => 2_500,
        5 => 4_000,
        6 => 7_000,
        7 => 10_000,
        _ => 15_000,
    };
    let jitter = rand::thread_rng().gen_range(0..400);
    Duration::from_millis(base + jitter)
}

fn sample_text(bytes: &[u8]) -> String {
    let end = bytes.len().min(8_000);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

pub(crate) fn is_cloudflare_challenge(
    status: u16,
    cf_mitigated: bool,
    body_len: usize,
    sample: &str,
) -> bool {
    if cf_mitigated {
        return true;
    }
    let marker = sample.contains("Just a moment")
        || sample.contains("Performing security verification")
        || sample.contains("/cdn-cgi/challenge-platform/");
    if !marker {
        return false;
    }
    if (200..300).contains(&status) {
        return body_len < 50_000;
    }
    true
}

fn ensure_curl_impersonate() -> eyre::Result<PathBuf> {
    static CURL: Mutex<Option<PathBuf>> = Mutex::new(None);
    let mut guard = CURL.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(path) = guard.clone() {
        return Ok(path);
    }
    let path = locate_curl_impersonate()?;
    *guard = Some(path.clone());
    Ok(path)
}

fn locate_curl_impersonate() -> eyre::Result<PathBuf> {
    if let Some(path) = std::env::var_os("RR_CURL_IMPERSONATE") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        eyre::bail!("RR_CURL_IMPERSONATE={} is not a file", path.display());
    }
    if let Some(path) = find_on_path("curl-impersonate") {
        return Ok(path);
    }
    let dir = crate::cache::Cache::cache_path()?.join("curl-impersonate");
    let bin = dir.join("curl-impersonate");
    if bin.is_file() {
        return Ok(bin);
    }
    download_curl_impersonate(&dir)?;
    if bin.is_file() {
        Ok(bin)
    } else {
        eyre::bail!(
            "downloaded curl-impersonate but {} is missing",
            bin.display()
        );
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

fn download_curl_impersonate(dir: &Path) -> eyre::Result<()> {
    let Some(asset) = bundled_asset() else {
        eyre::bail!(
            "No curl-impersonate build is known for this platform. Install it and set RR_CURL_IMPERSONATE to the binary (Chrome 152 impersonation is required)."
        );
    };
    std::fs::create_dir_all(dir)?;
    let archive = dir.join(asset.name);
    let url = format!(
        "https://github.com/lexiforest/curl-impersonate/releases/download/{RELEASE}/{}",
        asset.name
    );
    tracing::info!("Downloading curl-impersonate {RELEASE} ({})", asset.name);
    let status = Command::new("curl")
        .args(["-fL", "--retry", "3", "--retry-delay", "2", "-o"])
        .arg(&archive)
        .arg(&url)
        .status()
        .map_err(|err| {
            eyre::eyre!("failed to run system curl to download curl-impersonate: {err}")
        })?;
    if !status.success() {
        eyre::bail!("failed to download {url}");
    }
    let digest = file_sha256(&archive)?;
    if !digest.eq_ignore_ascii_case(asset.sha256) {
        let _ = std::fs::remove_file(&archive);
        eyre::bail!(
            "curl-impersonate checksum mismatch for {}: got {digest}, expected {}",
            asset.name,
            asset.sha256
        );
    }
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(dir)
        .status()?;
    if !status.success() {
        eyre::bail!("failed to extract {}", archive.display());
    }
    let _ = std::fs::remove_file(&archive);
    Ok(())
}

fn file_sha256(path: &Path) -> eyre::Result<String> {
    let output = if Command::new("sha256sum").arg(path).output().is_ok() {
        Command::new("sha256sum").arg(path).output()?
    } else {
        Command::new("shasum")
            .args(["-a", "256"])
            .arg(path)
            .output()?
    };
    if !output.status.success() {
        eyre::bail!("failed to hash {}", path.display());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| eyre::eyre!("empty checksum for {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::is_cloudflare_challenge;

    #[test]
    fn challenge_header_and_interstitial_are_detected() {
        assert!(is_cloudflare_challenge(403, true, 6_000, "<html>"));
        assert!(is_cloudflare_challenge(
            403,
            false,
            6_000,
            "<title>Just a moment...</title>"
        ));
        assert!(is_cloudflare_challenge(
            503,
            false,
            8_000,
            "Performing security verification"
        ));
    }

    #[test]
    fn story_text_and_ordinary_errors_are_not_challenges() {
        assert!(!is_cloudflare_challenge(
            200,
            false,
            120_000,
            "Just a moment, the hero said"
        ));
        assert!(!is_cloudflare_challenge(404, false, 2_000, "not found"));
        assert!(!is_cloudflare_challenge(200, false, 90_000, "<p>Hello</p>"));
    }
}
