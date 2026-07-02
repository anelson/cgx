use std::{io::Read, path::Path, time::Duration};

use backon::{BlockingRetryable, ExponentialBuilder};
use bytes::Bytes;
pub(crate) use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{
    StatusCode,
    blocking::{Client, Response},
};
use snafu::ResultExt;

use crate::{Result, config::HttpConfig, error};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const API_RESPONSE_LIMIT_BYTES: usize = 1024 * 1024;
pub(crate) const SMALL_DOWNLOAD_LIMIT_BYTES: usize = 64 * 1024;

/// Build the cgx user agent string.
///
/// This is shared between [`HttpClient`] (for reqwest-based HTTP) and
/// the git client (injected into gix via config overrides).
pub(crate) fn user_agent() -> String {
    format!(
        "cgx/{} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY")
    )
}

/// HTTP client wrapper with retry, user agent, proxy, and timeout support.
///
/// This provides a unified HTTP client for all cgx HTTP operations including:
/// - Registry queries (sparse index)
/// - Binary downloads from providers
/// - API calls to GitHub/GitLab
///
/// Git operations use their own transport layer via `gix` and do not use this client directly.
/// cgx mirrors `HttpConfig` settings into gix where possible: proxy, user agent, retry/backoff,
/// and timeout (used as both connect timeout and stalled-transfer timeout threshold). See
/// [`crate::git`] for details.
#[derive(Debug, Clone)]
pub(crate) struct HttpClient {
    client: Client,
    config: HttpConfig,
}

impl HttpClient {
    /// Build a new [`HttpClient`] with the given configuration.
    pub(crate) fn new(config: &HttpConfig) -> Result<Self> {
        let mut builder = Client::builder()
            .user_agent(user_agent())
            .timeout(config.timeout)
            .connect_timeout(CONNECT_TIMEOUT);

        if let Some(ref proxy_url) = config.proxy {
            let proxy = reqwest::Proxy::all(proxy_url).map_err(|e| error::Error::HttpClientBuild {
                message: format!("invalid proxy URL '{}': {}", proxy_url, e),
            })?;
            builder = builder.proxy(proxy);
        }

        let client = builder.build().map_err(|e| error::Error::HttpClientBuild {
            message: e.to_string(),
        })?;

        Ok(Self {
            client,
            config: config.clone(),
        })
    }

    /// Get a reference to the inner [`reqwest::blocking::Client`].
    ///
    /// This is provided for use with [`tame_index::index::RemoteSparseIndex`] which
    /// requires a client reference.
    pub(crate) fn inner(&self) -> &Client {
        &self.client
    }

    /// Perform a GET request with retry on transient errors.
    ///
    /// Retries on certain errors that are considered retriable.
    ///
    /// Returns the response even if the status is non-success (except for retryable statuses);
    /// callers must inspect the status and handle non-2xx as needed.
    pub(crate) fn get(&self, url: &str) -> Result<Response> {
        self.get_with_headers(url, &HeaderMap::new())
    }

    /// Perform a GET request with custom headers and retry on transient errors.
    ///
    /// Retries on 429 (rate limit), 5xx (server errors), and connection errors.
    /// Returns the response even if the status is non-success (except for retryable statuses);
    /// callers must inspect the status and handle non-2xx as needed.
    pub(crate) fn get_with_headers(&self, url: &str, headers: &HeaderMap) -> Result<Response> {
        self.get_with_headers_retrying_status(url, headers, Self::standard_should_retry_status)
    }

    /// Perform a GET request with a custom status retry policy.
    ///
    /// Transport errors continue to use the standard retry policy; `should_retry_status` controls
    /// only HTTP response statuses.
    pub(crate) fn get_retrying_status(
        &self,
        url: &str,
        should_retry_status: fn(StatusCode) -> bool,
    ) -> Result<Response> {
        self.get_with_headers_retrying_status(url, &HeaderMap::new(), should_retry_status)
    }

    /// Perform a GET request with custom headers and a custom status retry policy.
    ///
    /// Transport errors continue to use the standard retry policy; `should_retry_status` controls
    /// only HTTP response statuses.
    pub(crate) fn get_with_headers_retrying_status(
        &self,
        url: &str,
        headers: &HeaderMap,
        should_retry_status: fn(StatusCode) -> bool,
    ) -> Result<Response> {
        let backoff = self.build_backoff();
        let url_owned = url.to_string();
        let headers = headers.clone();

        let operation = || {
            let mut request = self.client.get(&url_owned);
            for (key, value) in &headers {
                request = request.header(key, value);
            }

            let response = request.send().with_context(|_| error::HttpRequestSnafu {
                url: url_owned.clone(),
            })?;

            Self::classify_retryable_status(response, &url_owned, should_retry_status)
        };

        operation
            .retry(backoff)
            .when(error::Error::is_retryable_http_error)
            .notify(|err, dur| {
                tracing::debug!("HTTP request failed, retrying in {:?}: {:?}", dur, err);
            })
            .call()
    }

    /// Perform a HEAD request with a custom status retry policy.
    ///
    /// Transport errors continue to use the standard retry policy; `should_retry_status` controls
    /// only HTTP response statuses.
    pub(crate) fn head_retrying_status(
        &self,
        url: &str,
        should_retry_status: fn(StatusCode) -> bool,
    ) -> Result<Response> {
        let backoff = self.build_backoff();
        let url_owned = url.to_string();

        let operation = || {
            let response = self
                .client
                .head(&url_owned)
                .send()
                .with_context(|_| error::HttpRequestSnafu {
                    url: url_owned.clone(),
                })?;

            Self::classify_retryable_status(response, &url_owned, should_retry_status)
        };

        operation
            .retry(backoff)
            .when(error::Error::is_retryable_http_error)
            .notify(|err, dur| {
                tracing::debug!("HTTP HEAD request failed, retrying in {:?}: {:?}", dur, err);
            })
            .call()
    }

    /// Attempt to download a URL to a file with retry.
    ///
    /// Returns `Ok(true)` on success, `Ok(false)` if the server returned 404 (resource does not
    /// exist), or `Err` for any other failure.
    pub(crate) fn try_download_to_file(&self, url: &str, path: &Path) -> Result<bool> {
        self.try_download_to_file_retrying_status(url, path, Self::standard_should_retry_status)
    }

    /// Attempt to download a URL to a file using a custom HTTP status retry policy.
    ///
    /// Returns `Ok(true)` on success, `Ok(false)` if the server returned 404 (resource does not
    /// exist), or `Err` for any other failure.
    pub(crate) fn try_download_to_file_retrying_status(
        &self,
        url: &str,
        path: &Path,
        should_retry_status: fn(StatusCode) -> bool,
    ) -> Result<bool> {
        let response = self.get_retrying_status(url, should_retry_status)?;

        if response.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }

        if !response.status().is_success() {
            return error::HttpStatusSnafu {
                url: url.to_string(),
                status: response.status().as_u16(),
            }
            .fail();
        }

        Self::response_body_to_file(response, url, path)?;

        Ok(true)
    }

    /// Attempt to download a URL into memory, failing if the body exceeds `max_bytes`.
    ///
    /// Returns `Ok(Some(bytes))` on success, `Ok(None)` if the server returned 404 (resource does
    /// not exist), or `Err` for any other failure.
    pub(crate) fn try_download_bytes(&self, url: &str, max_bytes: usize) -> Result<Option<Bytes>> {
        self.try_download_bytes_retrying_status(url, max_bytes, Self::standard_should_retry_status)
    }

    /// Attempt to download a URL into memory using a custom HTTP status retry policy.
    ///
    /// Returns `Ok(Some(bytes))` on success, `Ok(None)` if the server returned 404 (resource does
    /// not exist), or `Err` for any other failure.
    pub(crate) fn try_download_bytes_retrying_status(
        &self,
        url: &str,
        max_bytes: usize,
        should_retry_status: fn(StatusCode) -> bool,
    ) -> Result<Option<Bytes>> {
        let response = self.get_retrying_status(url, should_retry_status)?;

        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            return error::HttpStatusSnafu {
                url: url.to_string(),
                status: response.status().as_u16(),
            }
            .fail();
        }

        Self::response_body_to_bytes(response, url, max_bytes).map(Some)
    }

    pub(crate) fn response_body_to_file(mut response: Response, url: &str, path: &Path) -> Result<()> {
        let mut file = std::fs::File::create(path).with_context(|_| error::IoSnafu {
            path: path.to_path_buf(),
        })?;

        std::io::copy(&mut response, &mut file).map_err(|source| error::Error::HttpDownloadToFile {
            url: url.to_string(),
            path: path.to_path_buf(),
            source,
        })?;

        Ok(())
    }

    pub(crate) fn response_body_to_string(response: Response, url: &str, max_bytes: usize) -> Result<String> {
        let bytes = Self::response_body_to_bytes(response, url, max_bytes)?;
        String::from_utf8(bytes.to_vec()).map_err(|source| error::Error::HttpResponseUtf8 {
            url: url.to_string(),
            source,
        })
    }

    fn response_body_to_bytes(response: Response, url: &str, max_bytes: usize) -> Result<Bytes> {
        let mut bytes = Vec::new();
        let read_limit = u64::try_from(max_bytes).unwrap_or(u64::MAX - 1).saturating_add(1);
        let mut limited = response.take(read_limit);

        limited
            .read_to_end(&mut bytes)
            .map_err(|source| error::Error::HttpResponseRead {
                url: url.to_string(),
                source,
            })?;

        if bytes.len() > max_bytes {
            return error::HttpResponseTooLargeSnafu {
                url: url.to_string(),
                limit: max_bytes,
            }
            .fail();
        }

        Ok(Bytes::from(bytes))
    }

    fn build_backoff(&self) -> ExponentialBuilder {
        ExponentialBuilder::default()
            .with_min_delay(self.config.backoff_base)
            .with_max_delay(self.config.backoff_max)
            .with_max_times(self.config.retries)
            .with_jitter()
    }

    /// Return whether a status should be retried by the default HTTP policy.
    fn standard_should_retry_status(status: StatusCode) -> bool {
        status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
    }

    /// Convert retryable HTTP status codes into errors that trigger retry.
    ///
    /// Returns `Err(HttpStatus)` only for status codes that we consider retriable, so the
    /// retry policy can act on them. `Ok(response)` does not imply success; it only
    /// means the response is not retryable and should be handled by the caller.
    fn classify_retryable_status(
        response: Response,
        url: &str,
        should_retry_status: fn(StatusCode) -> bool,
    ) -> Result<Response> {
        let status = response.status();

        if should_retry_status(status) {
            return error::HttpStatusSnafu {
                url: url.to_string(),
                status: status.as_u16(),
            }
            .fail();
        }

        // All other responses are returned as-is.
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use super::*;

    #[test]
    fn test_construction_with_defaults() {
        let config = HttpConfig::default();
        let client = HttpClient::new(&config).unwrap();

        // Verify user agent contains version and repo
        let user_agent = format!(
            "cgx/{} ({})",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_REPOSITORY")
        );
        assert!(user_agent.contains("cgx/"));
        assert!(user_agent.contains("github.com"));

        // Client should be constructed without error (we can't easily assert much about it)
        let _inner = client.inner();
    }

    #[test]
    fn test_construction_with_http_proxy() {
        let config = HttpConfig {
            proxy: Some("http://localhost:8080".to_string()),
            ..Default::default()
        };
        HttpClient::new(&config).unwrap();
    }

    #[test]
    fn test_construction_with_socks_proxy() {
        let config = HttpConfig {
            proxy: Some("socks5://localhost:1080".to_string()),
            ..Default::default()
        };
        HttpClient::new(&config).unwrap();
    }

    #[test]
    fn test_construction_with_invalid_proxy() {
        let config = HttpConfig {
            proxy: Some("://invalid-no-scheme".to_string()),
            ..Default::default()
        };
        let result = HttpClient::new(&config);
        assert_matches!(result, Err(error::Error::HttpClientBuild { .. }));
    }

    #[test]
    fn test_construction_with_custom_timeout() {
        let config = HttpConfig {
            timeout: Duration::from_secs(120),
            ..Default::default()
        };
        HttpClient::new(&config).unwrap();
    }

    #[test]
    fn test_construction_with_zero_retries() {
        let config = HttpConfig {
            retries: 0,
            ..Default::default()
        };
        HttpClient::new(&config).unwrap();
    }

    #[test]
    fn test_construction_with_https_proxy() {
        let config = HttpConfig {
            proxy: Some("https://proxy.example.com:3128".to_string()),
            ..Default::default()
        };
        HttpClient::new(&config).unwrap();
    }

    #[test]
    fn test_construction_with_socks5h_proxy() {
        let config = HttpConfig {
            proxy: Some("socks5h://localhost:1080".to_string()),
            ..Default::default()
        };
        HttpClient::new(&config).unwrap();
    }

    fn fast_retry_config() -> HttpConfig {
        HttpConfig {
            retries: 2,
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(10),
            ..Default::default()
        }
    }

    mod classify_retryable_status_tests {
        use httpmock::prelude::*;

        use super::*;

        fn retry_server_errors_only(status: StatusCode) -> bool {
            status.is_server_error()
        }

        fn never_retry_status(_: StatusCode) -> bool {
            false
        }

        #[test]
        fn test_get_200_returned_directly() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.method(GET).path("/ok");
                then.status(200).body("success");
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let response = client.get(&server.url("/ok")).unwrap();
            assert_eq!(response.status(), 200);
            mock.assert_calls(1);
        }

        #[test]
        fn test_get_404_returned_not_retried() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.method(GET).path("/notfound");
                then.status(404);
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let response = client.get(&server.url("/notfound")).unwrap();
            assert_eq!(response.status(), 404);
            mock.assert_calls(1);
        }

        #[test]
        fn test_get_403_returned_not_retried() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.method(GET).path("/forbidden");
                then.status(403);
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let response = client.get(&server.url("/forbidden")).unwrap();
            assert_eq!(response.status(), 403);
            mock.assert_calls(1);
        }

        #[test]
        fn test_get_429_triggers_retry() {
            let server = MockServer::start();
            let fail_mock = server.mock(|when, then| {
                when.method(GET).path("/ratelimit");
                then.status(429);
            });

            let config = HttpConfig {
                retries: 1,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(10),
                ..Default::default()
            };
            let client = HttpClient::new(&config).unwrap();
            let result = client.get(&server.url("/ratelimit"));
            assert_matches!(result, Err(error::Error::HttpStatus { status: 429, .. }));
            fail_mock.assert_calls(2);
        }

        #[test]
        fn test_get_500_triggers_retry() {
            let server = MockServer::start();
            let fail_mock = server.mock(|when, then| {
                when.method(GET).path("/error");
                then.status(500);
            });

            let config = HttpConfig {
                retries: 1,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(10),
                ..Default::default()
            };
            let client = HttpClient::new(&config).unwrap();
            let result = client.get(&server.url("/error"));
            assert_matches!(result, Err(error::Error::HttpStatus { status: 500, .. }));
            fail_mock.assert_calls(2);
        }

        #[test]
        fn test_get_503_triggers_retry() {
            let server = MockServer::start();
            let fail_mock = server.mock(|when, then| {
                when.method(GET).path("/unavailable");
                then.status(503);
            });

            let config = HttpConfig {
                retries: 1,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(10),
                ..Default::default()
            };
            let client = HttpClient::new(&config).unwrap();
            let result = client.get(&server.url("/unavailable"));
            assert_matches!(result, Err(error::Error::HttpStatus { status: 503, .. }));
            fail_mock.assert_calls(2);
        }

        #[test]
        fn test_exhausted_retries_returns_error() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.method(GET).path("/always-fail");
                then.status(503);
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let result = client.get(&server.url("/always-fail"));
            assert_matches!(result, Err(error::Error::HttpStatus { status: 503, .. }));
            mock.assert_calls(3);
        }

        #[test]
        fn test_zero_retries_no_retry_on_5xx() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.method(GET).path("/once");
                then.status(500);
            });

            let config = HttpConfig {
                retries: 0,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(10),
                ..Default::default()
            };
            let client = HttpClient::new(&config).unwrap();
            let result = client.get(&server.url("/once"));
            assert_matches!(result, Err(error::Error::HttpStatus { status: 500, .. }));
            mock.assert_calls(1);
        }

        #[test]
        fn test_custom_status_retry_predicate_returns_429_without_retrying() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.method(GET).path("/ratelimit");
                then.status(429);
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let response = client
                .get_retrying_status(&server.url("/ratelimit"), retry_server_errors_only)
                .unwrap();
            assert_eq!(response.status(), 429);
            mock.assert_calls(1);
        }

        #[test]
        fn test_custom_status_retry_predicate_still_retries_5xx() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.method(GET).path("/error");
                then.status(500);
            });

            let config = HttpConfig {
                retries: 1,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(10),
                ..Default::default()
            };
            let client = HttpClient::new(&config).unwrap();
            let result = client.get_retrying_status(&server.url("/error"), retry_server_errors_only);
            assert_matches!(result, Err(error::Error::HttpStatus { status: 500, .. }));
            mock.assert_calls(2);
        }

        #[test]
        fn test_custom_status_retry_predicate_still_retries_transport_errors() {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/closed", listener.local_addr().unwrap());
            let request_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let accepting = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let request_count_for_thread = std::sync::Arc::clone(&request_count);
            let accepting_for_thread = std::sync::Arc::clone(&accepting);
            let server = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while accepting_for_thread.load(std::sync::atomic::Ordering::SeqCst)
                    && std::time::Instant::now() < deadline
                {
                    match listener.accept() {
                        Ok((stream, _addr)) => {
                            request_count_for_thread.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            drop(stream);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });

            let config = HttpConfig {
                retries: 1,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(10),
                ..Default::default()
            };
            let client = HttpClient::new(&config).unwrap();
            let result = client.get_retrying_status(&url, never_retry_status);
            accepting.store(false, std::sync::atomic::Ordering::SeqCst);
            server.join().unwrap();

            assert_matches!(result, Err(error::Error::HttpRequest { .. }));
            assert!(
                request_count.load(std::sync::atomic::Ordering::SeqCst) >= 2,
                "transport errors should still use the standard retry path"
            );
        }
    }

    mod download_tests {
        use httpmock::{HttpMockRequest, HttpMockResponse, prelude::*};

        use super::*;

        const TEST_DOWNLOAD_LIMIT_BYTES: usize = 12;

        #[test]
        fn test_try_download_to_file_200_writes_file_and_returns_true() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET).path("/binary");
                then.status(200).body("file-content");
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let file = tempfile::NamedTempFile::new().unwrap();
            let result = client
                .try_download_to_file(&server.url("/binary"), file.path())
                .unwrap();
            assert!(result);
            assert_eq!(std::fs::read_to_string(file.path()).unwrap(), "file-content");
        }

        #[test]
        fn test_try_download_to_file_404_returns_false() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET).path("/missing");
                then.status(404);
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let file = tempfile::NamedTempFile::new().unwrap();
            let result = client
                .try_download_to_file(&server.url("/missing"), file.path())
                .unwrap();
            assert!(!result);
        }

        #[test]
        fn test_try_download_to_file_403_returns_error() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET).path("/denied");
                then.status(403);
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let file = tempfile::NamedTempFile::new().unwrap();
            let result = client.try_download_to_file(&server.url("/denied"), file.path());
            assert_matches!(result, Err(error::Error::HttpStatus { status: 403, .. }));
        }

        #[test]
        fn test_try_download_to_file_retries_then_succeeds() {
            let server = MockServer::start();
            let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let call_count_for_mock = std::sync::Arc::clone(&call_count);

            let mock = server.mock(|when, then| {
                when.method(GET).path("/flaky");
                then.respond_with(move |_req: &HttpMockRequest| {
                    let attempt = call_count_for_mock.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if attempt == 0 {
                        HttpMockResponse::builder().status(500).build()
                    } else {
                        HttpMockResponse::builder().status(200).body("recovered").build()
                    }
                });
            });

            let retry_config = fast_retry_config();
            let retry_client = HttpClient::new(&retry_config).unwrap();
            let file = tempfile::NamedTempFile::new().unwrap();
            let result = retry_client
                .try_download_to_file(&server.url("/flaky"), file.path())
                .unwrap();
            assert!(result);
            assert_eq!(std::fs::read_to_string(file.path()).unwrap(), "recovered");
            mock.assert_calls(2);
        }

        #[test]
        fn test_try_download_bytes_200_returns_some_bytes() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET).path("/binary");
                then.status(200).body("file-content");
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let result = client
                .try_download_bytes(&server.url("/binary"), TEST_DOWNLOAD_LIMIT_BYTES)
                .unwrap();
            assert_eq!(result, Some(Bytes::from("file-content")));
        }

        #[test]
        fn test_try_download_bytes_404_returns_none() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET).path("/missing");
                then.status(404);
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let result = client
                .try_download_bytes(&server.url("/missing"), TEST_DOWNLOAD_LIMIT_BYTES)
                .unwrap();
            assert_eq!(result, None);
        }

        #[test]
        fn test_try_download_bytes_exact_limit_succeeds() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET).path("/exact");
                then.status(200).body("123456789012");
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let result = client
                .try_download_bytes(&server.url("/exact"), TEST_DOWNLOAD_LIMIT_BYTES)
                .unwrap();
            assert_eq!(result, Some(Bytes::from("123456789012")));
        }

        #[test]
        fn test_try_download_bytes_over_limit_errors() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET).path("/too-large");
                then.status(200).body("1234567890123");
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let result = client.try_download_bytes(&server.url("/too-large"), TEST_DOWNLOAD_LIMIT_BYTES);
            assert_matches!(result, Err(error::Error::HttpResponseTooLarge { .. }));
        }
    }

    mod header_tests {
        use httpmock::{Method::HEAD, prelude::*};

        use super::*;

        #[test]
        fn test_user_agent_header_sent() {
            let server = MockServer::start();
            let expected_ua = format!(
                "cgx/{} ({})",
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_REPOSITORY")
            );
            let mock = server.mock(|when, then| {
                when.method(GET)
                    .path("/ua-check")
                    .header("user-agent", &expected_ua);
                then.status(200);
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            client.get(&server.url("/ua-check")).unwrap();
            mock.assert();
        }

        #[test]
        fn test_get_with_headers_sends_custom_headers() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.method(GET)
                    .path("/auth-check")
                    .header("authorization", "Bearer token123")
                    .header("accept", "application/json");
                then.status(200);
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer token123"));
            headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
            client
                .get_with_headers(&server.url("/auth-check"), &headers)
                .unwrap();
            mock.assert();
        }

        #[test]
        fn test_head_sends_head_request() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.method(HEAD).path("/head-check");
                then.status(200);
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let response = client
                .head_retrying_status(
                    &server.url("/head-check"),
                    HttpClient::standard_should_retry_status,
                )
                .unwrap();
            assert_eq!(response.status(), 200);
            mock.assert();
        }

        #[test]
        fn test_head_retries_on_429() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.method(HEAD).path("/head-ratelimit");
                then.status(429);
            });

            let config = HttpConfig {
                retries: 1,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(10),
                ..Default::default()
            };
            let client = HttpClient::new(&config).unwrap();
            let result = client.head_retrying_status(
                &server.url("/head-ratelimit"),
                HttpClient::standard_should_retry_status,
            );
            assert_matches!(result, Err(error::Error::HttpStatus { status: 429, .. }));
            mock.assert_calls(2);
        }

        #[test]
        fn test_head_retries_on_5xx() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.method(HEAD).path("/head-error");
                then.status(500);
            });

            let config = HttpConfig {
                retries: 1,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(10),
                ..Default::default()
            };
            let client = HttpClient::new(&config).unwrap();
            let result = client.head_retrying_status(
                &server.url("/head-error"),
                HttpClient::standard_should_retry_status,
            );
            assert_matches!(result, Err(error::Error::HttpStatus { status: 500, .. }));
            mock.assert_calls(2);
        }
    }
}
