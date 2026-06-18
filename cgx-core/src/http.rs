use std::time::Duration;

use backon::{BlockingRetryable, ExponentialBuilder};
pub use bytes::Bytes;
use reqwest::blocking::{Client, Response};
pub use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use snafu::ResultExt;

use crate::{Result, config::HttpConfig, error};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

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
pub struct HttpClient {
    client: Client,
    config: HttpConfig,
}

impl HttpClient {
    /// Build a new [`HttpClient`] with the given configuration.
    pub fn new(config: &HttpConfig) -> Result<Self> {
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
    pub fn inner(&self) -> &Client {
        &self.client
    }

    /// Perform a GET request with retry on transient errors.
    ///
    /// Retries on certain errors that are considered retriable.
    ///
    /// Returns the response even if the status is non-success (except for retryable statuses);
    /// callers must inspect the status and handle non-2xx as needed.
    pub fn get(&self, url: &str) -> Result<Response> {
        self.get_with_headers(url, &HeaderMap::new())
    }

    /// Perform a GET request with custom headers and retry on transient errors.
    ///
    /// Retries on 429 (rate limit), 5xx (server errors), and connection errors.
    /// Returns the response even if the status is non-success (except for retryable statuses);
    /// callers must inspect the status and handle non-2xx as needed.
    pub fn get_with_headers(&self, url: &str, headers: &HeaderMap) -> Result<Response> {
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

            Self::classify_retryable_status(response, &url_owned)
        };

        operation
            .retry(backoff)
            .when(Self::is_retryable_error)
            .notify(|err, dur| {
                tracing::debug!("HTTP request failed, retrying in {:?}: {:?}", dur, err);
            })
            .call()
    }

    /// Perform a HEAD request with retry on transient errors.
    ///
    /// Retries on 429 (rate limit), 5xx (server errors), and connection errors.
    /// Returns the response even if the status is non-success (except for retryable statuses);
    /// callers must inspect the status and handle non-2xx as needed.
    pub fn head(&self, url: &str) -> Result<Response> {
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

            Self::classify_retryable_status(response, &url_owned)
        };

        operation
            .retry(backoff)
            .when(Self::is_retryable_error)
            .notify(|err, dur| {
                tracing::debug!("HTTP HEAD request failed, retrying in {:?}: {:?}", dur, err);
            })
            .call()
    }

    /// Attempt to download a file from the given URL with retry.
    ///
    /// Returns `Ok(Some(bytes))` on success, `Ok(None)` if the server returned 404
    /// (resource does not exist), or `Err` for any other failure (network errors,
    /// non-404 HTTP errors after retries).
    ///
    /// This is a convenience method that encapsulates the common pattern used by
    /// all binary providers.
    pub fn try_download(&self, url: &str) -> Result<Option<Bytes>> {
        let response = self.get(url)?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            return error::HttpStatusSnafu {
                url: url.to_string(),
                status: response.status().as_u16(),
            }
            .fail();
        }

        let bytes = response
            .bytes()
            .with_context(|_| error::HttpRequestSnafu { url: url.to_string() })?;

        Ok(Some(bytes))
    }

    /// Check if an error indicates a connection/timeout failure (vs a logical HTTP error).
    ///
    /// This is used by the GitLab provider to bail early when the server is unreachable,
    /// rather than continuing to probe ~160 candidate URLs against a dead server.
    pub fn is_connection_error(err: &error::Error) -> bool {
        match err {
            error::Error::HttpRequest { source, .. } => {
                source.is_connect() || source.is_timeout() || source.is_request()
            }
            _ => false,
        }
    }

    fn build_backoff(&self) -> ExponentialBuilder {
        ExponentialBuilder::default()
            .with_min_delay(self.config.backoff_base)
            .with_max_delay(self.config.backoff_max)
            .with_max_times(self.config.retries)
            .with_jitter()
    }

    /// Convert retryable HTTP status codes into errors that trigger retry.
    ///
    /// Returns `Err(HttpStatus)` only for status codes that we consider retriable, so the
    /// retry policy can act on them. `Ok(response)` does not imply success; it only
    /// means the response is not retryable and should be handled by the caller.
    fn classify_retryable_status(response: Response, url: &str) -> Result<Response> {
        let status = response.status();

        // 429 Too Many Requests - retryable
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return error::HttpStatusSnafu {
                url: url.to_string(),
                status: status.as_u16(),
            }
            .fail();
        }

        // 5xx Server Errors - retryable
        if status.is_server_error() {
            return error::HttpStatusSnafu {
                url: url.to_string(),
                status: status.as_u16(),
            }
            .fail();
        }

        // All other responses (including 4xx other than 429) are returned as-is
        Ok(response)
    }

    fn is_retryable_error(err: &error::Error) -> bool {
        match err {
            error::Error::HttpStatus { status, .. } => {
                *status == reqwest::StatusCode::TOO_MANY_REQUESTS.as_u16() || *status >= 500
            }
            error::Error::HttpRequest { source, .. } => {
                source.is_connect() || source.is_timeout() || source.is_request()
            }
            _ => false,
        }
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
    fn test_is_connection_error() {
        // HttpStatus is not a connection error
        let status_err = error::Error::HttpStatus {
            url: "http://example.com".to_string(),
            status: 500,
        };
        assert!(!HttpClient::is_connection_error(&status_err));

        // HttpClientBuild is not a connection error
        let build_err = error::Error::HttpClientBuild {
            message: "test".to_string(),
        };
        assert!(!HttpClient::is_connection_error(&build_err));
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

    #[test]
    fn test_is_connection_error_various_non_http_errors() {
        let errors: Vec<error::Error> = vec![
            error::Error::HttpStatus {
                url: "http://example.com".to_string(),
                status: 429,
            },
            error::Error::HttpStatus {
                url: "http://example.com".to_string(),
                status: 503,
            },
            error::Error::HttpClientBuild {
                message: "bad config".to_string(),
            },
            error::Error::InvalidHttpTimeout {
                value: "not-a-duration".to_string(),
                source: humantime::parse_duration("not-a-duration").unwrap_err(),
            },
        ];
        for err in &errors {
            assert!(
                !HttpClient::is_connection_error(err),
                "Expected false for {:?}",
                err
            );
        }
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
    }

    mod try_download_tests {
        use httpmock::{HttpMockRequest, HttpMockResponse, prelude::*};

        use super::*;

        #[test]
        fn test_try_download_200_returns_some_bytes() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET).path("/binary");
                then.status(200).body("file-content");
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let result = client.try_download(&server.url("/binary")).unwrap();
            assert_eq!(result, Some(Bytes::from("file-content")));
        }

        #[test]
        fn test_try_download_404_returns_none() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET).path("/missing");
                then.status(404);
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let result = client.try_download(&server.url("/missing")).unwrap();
            assert_eq!(result, None);
        }

        #[test]
        fn test_try_download_403_returns_error() {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(GET).path("/denied");
                then.status(403);
            });

            let client = HttpClient::new(&fast_retry_config()).unwrap();
            let result = client.try_download(&server.url("/denied"));
            assert_matches!(result, Err(error::Error::HttpStatus { status: 403, .. }));
        }

        #[test]
        fn test_try_download_retries_then_succeeds() {
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
            let result = retry_client.try_download(&server.url("/flaky")).unwrap();
            assert_eq!(result, Some(Bytes::from("recovered")));
            mock.assert_calls(2);
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
            let response = client.head(&server.url("/head-check")).unwrap();
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
            let result = client.head(&server.url("/head-ratelimit"));
            assert_matches!(result, Err(error::Error::HttpStatus { status: 429, .. }));
            mock.assert_calls(2);
        }
    }
}
