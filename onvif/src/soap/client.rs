#![allow(clippy::large_enum_variant)]

use crate::soap::{
    self,
    auth::{digest::Digest, username_token::UsernameToken},
};
use async_recursion::async_recursion;
use async_trait::async_trait;
use futures_util::lock::Mutex;
use schema::transport::{Error, Transport};
use std::ops::DerefMut;
use std::{
    fmt::{Debug, Formatter},
    sync::Arc,
    time::Duration,
};
use tracing::{debug, instrument, trace};
use url::Url;

#[derive(Clone)]
pub struct Client {
    client: reqwest::Client,
    config: Config,
    digest_auth_state: Arc<Mutex<Digest>>,
}

#[derive(Clone)]
pub struct ClientBuilder {
    client: Option<reqwest::Client>,
    config: Config,
}

impl ClientBuilder {
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
    pub const DEFAULT_MAX_REDIRECTIONS: u32 = 1;
    pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

    pub fn new(uri: &Url) -> Self {
        Self {
            client: None,
            config: Config {
                uri: uri.clone(),
                credentials: None,
                response_patcher: None,
                auth_type: AuthType::Any,
                reuse_digest_auth_headers: false,
                timeout: ClientBuilder::DEFAULT_TIMEOUT,
                fix_time_gap: None,
                allow_username_token_over_http: false,
                max_redirections: ClientBuilder::DEFAULT_MAX_REDIRECTIONS,
                max_response_bytes: ClientBuilder::DEFAULT_MAX_RESPONSE_BYTES,
                #[cfg(feature = "tls")]
                tls: TlsConfig::default(),
            },
        }
    }

    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.client = Some(client);
        self
    }

    pub fn credentials(mut self, credentials: Option<Credentials>) -> Self {
        self.config.credentials = credentials;
        self
    }

    pub fn response_patcher(mut self, response_patcher: Option<ResponsePatcher>) -> Self {
        self.config.response_patcher = response_patcher;
        self
    }

    pub fn auth_type(mut self, auth_type: AuthType) -> Self {
        self.config.auth_type = auth_type;
        self
    }

    pub fn reuse_digest_auth_headers(mut self, reuse_digest_auth_headers: bool) -> Self {
        self.config.reuse_digest_auth_headers = reuse_digest_auth_headers;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.config.timeout = timeout;
        self
    }

    pub fn fix_time_gap(mut self, time_gap: Option<chrono::Duration>) -> Self {
        self.config.fix_time_gap = time_gap;
        self
    }

    /// How many redirects to follow for one request. Default:
    /// [ClientBuilder::DEFAULT_MAX_REDIRECTIONS].
    ///
    /// Redirects carry the credentials, so only redirects to the same host are followed, and
    /// never from HTTPS to HTTP.
    pub fn max_redirections(mut self, max_redirections: u32) -> Self {
        self.config.max_redirections = max_redirections;
        self
    }

    /// The largest response body to read, in bytes. Larger responses fail with a protocol error
    /// instead of being read into memory. Default: [ClientBuilder::DEFAULT_MAX_RESPONSE_BYTES].
    pub fn max_response_bytes(mut self, max_response_bytes: usize) -> Self {
        self.config.max_response_bytes = max_response_bytes;
        self
    }

    /// Allows WS-Security UsernameToken auth on plain `http://` URIs.
    ///
    /// Off by default: over plain HTTP, anyone who captures a UsernameToken request can guess the
    /// password offline from its SHA-1 digest, and ONVIF advises against it. Without this, the
    /// client uses UsernameToken only over HTTPS, and [AuthType::Any] does not fall back to it
    /// over HTTP. Turn it on for cameras that support nothing else, on a network you trust.
    pub fn allow_username_token_over_http(mut self, allow: bool) -> Self {
        self.config.allow_username_token_over_http = allow;
        self
    }

    /// Trusts `certificate` as a root CA for HTTPS, in addition to the system roots.
    ///
    /// Use this for a site CA or for the camera's own self-signed certificate (pinning). Ignored
    /// if a client is given with [ClientBuilder::http_client].
    #[cfg(feature = "tls")]
    pub fn add_root_certificate(mut self, certificate: reqwest::Certificate) -> Self {
        self.config.tls.root_certificates.push(certificate);
        self
    }

    /// Accepts any HTTPS certificate, including expired, self-signed and forged ones.
    ///
    /// Off by default. Turning it on makes HTTPS open to man-in-the-middle attacks, so prefer
    /// [ClientBuilder::add_root_certificate]. Ignored if a client is given with
    /// [ClientBuilder::http_client].
    #[cfg(feature = "tls")]
    pub fn danger_accept_invalid_certs(mut self, accept_invalid_certs: bool) -> Self {
        self.config.tls.accept_invalid_certs = accept_invalid_certs;
        self
    }

    /// Accepts HTTPS certificates whose names do not match the host, while still checking that a
    /// trusted CA signed them.
    ///
    /// Off by default. Cameras are often reached by an IP address that is missing from their
    /// certificate; this allows that with a pinned certificate. Ignored if a client is given with
    /// [ClientBuilder::http_client].
    #[cfg(feature = "tls")]
    pub fn danger_accept_invalid_hostnames(mut self, accept_invalid_hostnames: bool) -> Self {
        self.config.tls.accept_invalid_hostnames = accept_invalid_hostnames;
        self
    }

    /// Builds the client.
    ///
    /// # Panics
    ///
    /// If the HTTP client cannot be built, for example when the TLS backend fails to initialize.
    /// Use [ClientBuilder::try_build] to handle that as an error.
    pub fn build(self) -> Client {
        self.try_build().expect("Failed to build the HTTP client")
    }

    /// Builds the client, or returns the error if the HTTP client cannot be built.
    pub fn try_build(self) -> Result<Client, reqwest::Error> {
        let client = if let Some(client) = self.client {
            client
        } else {
            #[allow(unused_mut)]
            let mut builder =
                ClientBuilder::default_http_client_builder().timeout(self.config.timeout);

            #[cfg(feature = "tls")]
            {
                let tls = &self.config.tls;
                for certificate in &tls.root_certificates {
                    builder = builder.add_root_certificate(certificate.clone());
                }
                builder = builder
                    .danger_accept_invalid_certs(tls.accept_invalid_certs)
                    .danger_accept_invalid_hostnames(tls.accept_invalid_hostnames);
            }

            builder.build()?
        };

        let digest = Digest::new(
            &self.config.uri,
            &self.config.credentials,
            self.config.reuse_digest_auth_headers,
        );

        Ok(Client {
            client,
            config: self.config,
            digest_auth_state: Arc::new(Mutex::new(digest)),
        })
    }

    /// The HTTP client builder this crate starts from: redirects off (they are handled per
    /// request) and, with the `tls` feature, native TLS that verifies certificates.
    pub fn default_http_client_builder() -> reqwest::ClientBuilder {
        #[allow(unused_mut)]
        let mut client_builder =
            reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());

        #[cfg(feature = "tls")]
        {
            client_builder = client_builder.use_native_tls();
        }

        client_builder
    }
}

#[derive(Clone)]
struct Config {
    uri: Url,
    credentials: Option<Credentials>,
    response_patcher: Option<ResponsePatcher>,
    auth_type: AuthType,
    reuse_digest_auth_headers: bool,
    timeout: Duration,
    fix_time_gap: Option<chrono::Duration>,
    allow_username_token_over_http: bool,
    max_redirections: u32,
    max_response_bytes: usize,
    #[cfg(feature = "tls")]
    tls: TlsConfig,
}

#[cfg(feature = "tls")]
#[derive(Clone, Default)]
struct TlsConfig {
    root_certificates: Vec<reqwest::Certificate>,
    accept_invalid_certs: bool,
    accept_invalid_hostnames: bool,
}

#[derive(Clone, Debug)]
pub enum AuthType {
    /// First try to authorize with Digest and in case of error try UsernameToken auth.
    ///
    /// Over plain HTTP, the fallback to UsernameToken needs
    /// [ClientBuilder::allow_username_token_over_http].
    Any,
    /// Use only Digest auth
    Digest,
    /// Use only UsernameToken auth.
    ///
    /// Over plain HTTP, this needs [ClientBuilder::allow_username_token_over_http].
    UsernameToken,
}

#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Debug for Credentials {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{} [password hidden]", self.username))
    }
}

pub type ResponsePatcher = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

#[derive(Debug)]
enum RequestAuthType<'a> {
    Digest(&'a mut Digest),
    UsernameToken,
}

#[async_trait]
impl Transport for Client {
    #[instrument(skip_all, fields(uri = self.config.uri.as_str()))]
    async fn request(&self, message: &str) -> Result<String, Error> {
        match self.config.auth_type {
            AuthType::Any => match self.request_with_digest(message).await {
                Ok(success) => Ok(success),
                Err(Error::Authorization(digest_error)) => {
                    debug!("Failed to authorize with Digest auth: {digest_error}. Trying UsernameToken auth ...");
                    self.request_with_username_token(message)
                        .await
                        .map_err(|error| match error {
                            Error::Authorization(username_token_error) => {
                                Error::Authorization(format!(
                                    "Digest: {digest_error}, UsernameToken: {username_token_error}"
                                ))
                            }
                            error => error,
                        })
                }
                Err(e) => Err(e),
            },
            AuthType::Digest => self.request_with_digest(message).await,
            AuthType::UsernameToken => self.request_with_username_token(message).await,
        }
    }
}

impl Client {
    async fn request_with_digest(&self, message: &str) -> Result<String, Error> {
        let mut guard = self.digest_auth_state.lock().await;
        let mut auth_type = RequestAuthType::Digest(guard.deref_mut());

        self.request_recursive(message, &self.config.uri, &mut auth_type, 0)
            .await
    }

    async fn request_with_username_token(&self, message: &str) -> Result<String, Error> {
        let mut auth_type = RequestAuthType::UsernameToken;

        self.request_recursive(message, &self.config.uri, &mut auth_type, 0)
            .await
    }

    #[async_recursion]
    async fn request_recursive(
        &self,
        message: &str,
        uri: &Url,
        auth_type: &mut RequestAuthType,
        redirections: u32,
    ) -> Result<String, Error> {
        let username_token = match auth_type {
            RequestAuthType::UsernameToken => self.username_token_auth(),
            _ => None,
        };

        if username_token.is_some()
            && uri.scheme() != "https"
            && !self.config.allow_username_token_over_http
        {
            return Err(Error::Authorization(
                "UsernameToken over plain HTTP is disabled; use HTTPS or allow_username_token_over_http"
                    .to_string(),
            ));
        }

        debug!(?auth_type, %redirections, "About to make request.");

        let soap_msg =
            soap::soap(message, &username_token).map_err(|e| Error::Protocol(format!("{e:?}")))?;

        let mut request = self
            .client
            .post(uri.as_str())
            .header("Content-Type", "application/soap+xml; charset=utf-8;");

        if let RequestAuthType::Digest(digest) = auth_type {
            request = digest
                .add_headers_for(request, uri)
                .map_err(|e| Error::Authorization(e.to_string()))?;

            debug!("Digest headers added");
        }

        trace!("Request body: {soap_msg}");

        let response = request.body(soap_msg).send().await.map_err(|e| {
            let message = error_chain(&e);
            match e {
                e if e.is_connect() => Error::Connection(message),
                e if e.is_timeout() => Error::Timeout(message),
                e if e.is_redirect() => Error::Redirection(message),
                e if e.is_decode() || e.is_body() => Error::Protocol(message),
                _ => Error::Other(message),
            }
        })?;

        let status = response.status();

        debug!("Response status: {status}");

        if !status.is_success() && status != reqwest::StatusCode::UNAUTHORIZED {
            if let RequestAuthType::Digest(digest) = auth_type {
                digest.discard_challenge();
            }
        }

        if status.is_success() {
            if let RequestAuthType::Digest(digest) = auth_type {
                digest.set_success();
            }

            read_body(response, self.config.max_response_bytes)
                .await
                .and_then(|text| {
                    trace!("Response body: {text}");
                    let response =
                        soap::unsoap(&text).map_err(|e| Error::Protocol(format!("{e:?}")))?;
                    if let Some(response_patcher) = &self.config.response_patcher {
                        match response_patcher(&response) {
                            Ok(patched) => {
                                trace!("Response (SOAP unwrapped, patched): {patched}");
                                Ok(patched)
                            }
                            Err(e) => Err(Error::Protocol(format!("Patching failed: {e}"))),
                        }
                    } else {
                        Ok(response)
                    }
                })
        } else if status == reqwest::StatusCode::UNAUTHORIZED {
            match auth_type {
                RequestAuthType::Digest(digest) if !digest.is_failed() => {
                    digest.set_401(response);
                }
                _ => {
                    if let Ok(text) = read_body(response, self.config.max_response_bytes).await {
                        debug!("Got Unauthorized with body: {text}");
                    }

                    return Err(Error::Authorization(status.to_string()));
                }
            }

            self.request_recursive(message, uri, auth_type, redirections)
                .await
        } else if status.is_redirection() {
            // reqwest changes method on 302, so we have to handle redirections ourselves
            // https://github.com/seanmonstar/reqwest/issues/912

            if redirections >= self.config.max_redirections {
                return Err(Error::Redirection("Redirection limit exceeded".to_string()));
            }

            let new_url = Client::get_redirect_location(uri, &response)?;

            if !is_safe_redirect(uri, &new_url) {
                return Err(Error::Redirection(format!(
                    "Refusing to send credentials from {uri} to {new_url}: redirects must stay on \
                     the same host and must not go from HTTPS to HTTP"
                )));
            }

            debug!("Redirecting to {new_url} ...");

            self.request_recursive(message, &new_url, auth_type, redirections + 1)
                .await
        } else {
            let text = read_body(response, self.config.max_response_bytes)
                .await
                .unwrap_or_default();
            debug!("Got HTTP error with body: {text}");

            let Err(soap::Error::Fault(fault)) = soap::unsoap(&text) else {
                return Err(Error::Other(status.to_string()));
            };

            let message = match &fault.code.subcode {
                Some(subcode) => format!("{status} ({})", subcode.value),
                None => status.to_string(),
            };

            if fault.is_unauthorized() {
                Err(Error::Authorization(message))
            } else {
                Err(Error::Other(message))
            }
        }
    }

    fn get_redirect_location(uri: &Url, response: &reqwest::Response) -> Result<Url, Error> {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| Error::Redirection("Redirect without a Location header".to_string()))?
            .to_str()
            .map_err(|e| Error::Redirection(e.to_string()))?;

        // Location may be relative to the request URI.
        uri.join(location)
            .map_err(|e| Error::Redirection(e.to_string()))
    }

    pub fn username_token_auth(&self) -> Option<UsernameToken> {
        self.config
            .credentials
            .as_ref()
            .map(|c| UsernameToken::new(&c.username, &c.password, self.config.fix_time_gap))
    }

    pub fn set_fix_time_gap(&mut self, time_gap: Option<chrono::Duration>) {
        self.config.fix_time_gap = time_gap;
    }
}

/// Reads the response body as text, failing once it is longer than `limit` bytes, so a
/// misbehaving device cannot make the client buffer an unbounded body.
async fn read_body(mut response: reqwest::Response, limit: usize) -> Result<String, Error> {
    let too_large = || Error::Protocol(format!("Response body is larger than {limit} bytes"));

    if let Some(length) = response.content_length() {
        if length > limit as u64 {
            return Err(too_large());
        }
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Error::Protocol(error_chain(&e)))?
    {
        if body.len() + chunk.len() > limit {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }

    String::from_utf8(body).map_err(|e| Error::Protocol(e.to_string()))
}

/// Whether a redirect from `from` to `to` may carry the credentials: the same host, and no
/// downgrade from HTTPS to HTTP. The port may change, as it does from HTTP to HTTPS.
pub(crate) fn is_safe_redirect(from: &Url, to: &Url) -> bool {
    let same_host = from.host() == to.host();
    let downgrade = from.scheme() == "https" && to.scheme() != "https";
    let known_scheme = matches!(to.scheme(), "http" | "https");

    same_host && known_scheme && !downgrade
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    std::iter::successors(Some(error), |error| error.source())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}
