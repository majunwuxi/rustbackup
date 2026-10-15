use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

const AUTHORIZATION_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const USERINFO_ENDPOINT: &str = "https://openidconnect.googleapis.com/v1/userinfo";
const MAX_CALLBACK_BYTES: usize = 16 * 1024;
const TOKEN_REFRESH_SKEW: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct OAuthEndpoints {
    pub authorization: String,
    pub token: String,
    pub userinfo: String,
}

impl Default for OAuthEndpoints {
    fn default() -> Self {
        Self {
            authorization: AUTHORIZATION_ENDPOINT.to_owned(),
            token: TOKEN_ENDPOINT.to_owned(),
            userinfo: USERINFO_ENDPOINT.to_owned(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountIdentity {
    pub subject: String,
    pub email: Option<String>,
}

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("OAuth client configuration is invalid")]
    InvalidConfiguration,
    #[error("authorization callback timed out")]
    CallbackTimeout,
    #[error("authorization was denied or returned an invalid callback")]
    AuthorizationDenied,
    #[error("secure credential storage is unavailable")]
    SecureStoreUnavailable,
    #[error("secure credential storage operation failed")]
    SecureStoreFailure,
    #[error("Google authorization is required")]
    WaitingForAuthorization,
    #[error("Google {operation} failed (HTTP {status}; provider error: {code}; {description})")]
    ProviderFailure {
        operation: &'static str,
        status: u16,
        code: String,
        description: String,
    },
    #[error("Google request failed (HTTP {0})")]
    HttpStatus(u16),
    #[error("Google refresh token is no longer valid")]
    InvalidGrant,
    #[error("Google authorization service could not be reached")]
    Network,
    #[error("Google returned an invalid authorization response")]
    InvalidResponse,
    #[error("system clock is before the Unix epoch")]
    InvalidSystemTime,
    #[error("OAuth state was already consumed")]
    SessionConsumed,
}

/// Stores refresh tokens and resumable upload URLs in an operating-system
/// credential vault. Keys passed to this interface must never be logged.
pub trait CredentialStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, OAuthError>;
    fn put(&self, key: &str, value: &str) -> Result<(), OAuthError>;
    fn remove(&self, key: &str) -> Result<(), OAuthError>;
}

#[derive(Clone, Debug, Default)]
pub struct PlatformCredentialStore;

impl PlatformCredentialStore {
    pub fn new() -> Result<Self, OAuthError> {
        #[cfg(windows)]
        {
            Ok(Self)
        }
        #[cfg(not(windows))]
        {
            Err(OAuthError::SecureStoreUnavailable)
        }
    }
}

#[cfg(windows)]
impl CredentialStore for PlatformCredentialStore {
    fn get(&self, key: &str) -> Result<Option<String>, OAuthError> {
        let entry = credential_entry(key)?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(OAuthError::SecureStoreFailure),
        }
    }

    fn put(&self, key: &str, value: &str) -> Result<(), OAuthError> {
        credential_entry(key)?
            .set_password(value)
            .map_err(|_| OAuthError::SecureStoreFailure)
    }

    fn remove(&self, key: &str) -> Result<(), OAuthError> {
        match credential_entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(OAuthError::SecureStoreFailure),
        }
    }
}

#[cfg(windows)]
fn credential_entry(key: &str) -> Result<keyring::Entry, OAuthError> {
    if key.is_empty() || key.len() > 240 || key.chars().any(char::is_control) {
        return Err(OAuthError::InvalidConfiguration);
    }
    keyring::Entry::new("RustBackup.GoogleDrive", key).map_err(|_| OAuthError::SecureStoreFailure)
}

pub struct OAuthSession {
    listener: TcpListener,
    redirect_uri: String,
    state: String,
    verifier: String,
}

impl OAuthSession {
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Waits only on the IPv4 loopback listener created for this authorization
    /// request. The state is single-use and the callback body is never logged.
    pub fn wait_for_code(self, timeout: Duration) -> Result<AuthorizationCode, OAuthError> {
        let deadline = Instant::now() + timeout;
        self.listener
            .set_nonblocking(true)
            .map_err(|_| OAuthError::Network)?;
        loop {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .map_err(|_| OAuthError::Network)?;
                    let request = read_callback(&mut stream)?;
                    let parsed = Url::parse(&format!("http://127.0.0.1{request}"))
                        .map_err(|_| OAuthError::AuthorizationDenied)?;
                    if parsed.path() != "/" {
                        write_callback_response(&mut stream, false);
                        return Err(OAuthError::AuthorizationDenied);
                    }
                    let mut state = None;
                    let mut code = None;
                    let mut denied = false;
                    for (key, value) in parsed.query_pairs() {
                        match key.as_ref() {
                            "state" => state = Some(value.into_owned()),
                            "code" => code = Some(value.into_owned()),
                            "error" => denied = true,
                            _ => {}
                        }
                    }
                    let valid_state = state.as_deref().is_some_and(|value| {
                        constant_time_eq(value.as_bytes(), self.state.as_bytes())
                    });
                    let code = code.filter(|value| !value.is_empty() && value.len() <= 4096);
                    let accepted = valid_state && !denied && code.is_some();
                    write_callback_response(&mut stream, accepted);
                    if !accepted {
                        return Err(OAuthError::AuthorizationDenied);
                    }
                    return Ok(AuthorizationCode {
                        code: code.ok_or(OAuthError::AuthorizationDenied)?,
                        redirect_uri: self.redirect_uri,
                        verifier: self.verifier,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(OAuthError::CallbackTimeout);
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(_) => return Err(OAuthError::Network),
            }
        }
    }
}

pub struct AuthorizationCode {
    code: String,
    redirect_uri: String,
    verifier: String,
}

pub fn begin_authorization(
    client_id: &str,
    endpoints: &OAuthEndpoints,
) -> Result<(String, OAuthSession), OAuthError> {
    if client_id.trim().is_empty() || client_id.chars().any(char::is_control) {
        return Err(OAuthError::InvalidConfiguration);
    }
    let authorization_endpoint = validate_google_endpoint(
        &endpoints.authorization,
        "accounts.google.com",
        "/o/oauth2/v2/auth",
    )?;
    validate_google_endpoint(&endpoints.token, "oauth2.googleapis.com", "/token")?;
    validate_google_endpoint(
        &endpoints.userinfo,
        "openidconnect.googleapis.com",
        "/v1/userinfo",
    )?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|_| OAuthError::Network)?;
    let port = listener
        .local_addr()
        .map_err(|_| OAuthError::Network)?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let mut verifier_bytes = [0_u8; 64];
    let mut state_bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut verifier_bytes);
    OsRng.fill_bytes(&mut state_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let state = URL_SAFE_NO_PAD.encode(state_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));

    let mut authorization_url = authorization_endpoint;
    authorization_url
        .query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair(
            "scope",
            "openid email https://www.googleapis.com/auth/drive.file",
        )
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);

    Ok((
        authorization_url.into(),
        OAuthSession {
            listener,
            redirect_uri,
            state,
            verifier,
        },
    ))
}

struct CachedToken {
    account_id: String,
    access_token: String,
    expires_at: Instant,
}

pub struct OAuthClient {
    client_id: String,
    endpoints: OAuthEndpoints,
    credentials: Arc<dyn CredentialStore>,
    agent: ureq::Agent,
    cached: Mutex<Option<CachedToken>>,
}

#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: u64,
    pub refresh_token: Option<String>,
    pub refresh_token_expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct UserInfoResponse {
    sub: String,
    email: Option<String>,
}

impl OAuthClient {
    pub fn new(
        client_id: impl Into<String>,
        endpoints: OAuthEndpoints,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, OAuthError> {
        let client_id = client_id.into();
        if client_id.trim().is_empty() || client_id.chars().any(char::is_control) {
            return Err(OAuthError::InvalidConfiguration);
        }
        validate_google_endpoint(
            &endpoints.authorization,
            "accounts.google.com",
            "/o/oauth2/v2/auth",
        )?;
        validate_google_endpoint(&endpoints.token, "oauth2.googleapis.com", "/token")?;
        validate_google_endpoint(
            &endpoints.userinfo,
            "openidconnect.googleapis.com",
            "/v1/userinfo",
        )?;
        Ok(Self {
            client_id,
            endpoints,
            credentials,
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout_read(Duration::from_secs(30))
                .timeout_write(Duration::from_secs(30))
                .build(),
            cached: Mutex::new(None),
        })
    }

    pub fn complete_authorization(
        &self,
        authorization: AuthorizationCode,
    ) -> Result<AccountIdentity, OAuthError> {
        let token: TokenResponse = self.post_form(
            &self.endpoints.token,
            "authorization code exchange",
            &[
                ("code", authorization.code.as_str()),
                ("client_id", &self.client_id),
                ("code_verifier", authorization.verifier.as_str()),
                ("redirect_uri", authorization.redirect_uri.as_str()),
                ("grant_type", "authorization_code"),
            ],
        )?;
        if token.access_token.is_empty() || token.expires_in == 0 {
            return Err(OAuthError::InvalidResponse);
        }
        let identity: UserInfoResponse =
            self.get_json(&self.endpoints.userinfo, &token.access_token)?;
        if identity.sub.is_empty() || identity.sub.len() > 256 {
            return Err(OAuthError::InvalidResponse);
        }
        if let Some(refresh_token) = token.refresh_token.as_deref() {
            self.credentials
                .put(&refresh_key(&identity.sub), refresh_token)?;
        } else if self.credentials.get(&refresh_key(&identity.sub))?.is_none() {
            return Err(OAuthError::InvalidResponse);
        }
        if let Some(lifetime) = token.refresh_token_expires_in {
            self.save_refresh_expiry(&identity.sub, lifetime)?;
        }
        *self
            .cached
            .lock()
            .map_err(|_| OAuthError::SecureStoreFailure)? = Some(cached_token(
            &identity.sub,
            &token.access_token,
            token.expires_in,
        )?);
        Ok(AccountIdentity {
            subject: identity.sub,
            email: identity.email,
        })
    }

    pub fn access_token(&self, account_id: &str) -> Result<String, OAuthError> {
        let key = refresh_key(account_id);
        let now = Instant::now();
        {
            let cached = self
                .cached
                .lock()
                .map_err(|_| OAuthError::SecureStoreFailure)?;
            if let Some(token) = cached.as_ref().filter(|token| {
                token.account_id == account_id && token.expires_at > now + TOKEN_REFRESH_SKEW
            }) {
                return Ok(token.access_token.clone());
            }
        }
        if self.refresh_token_expired(account_id)? {
            self.credentials.remove(&key)?;
            self.credentials.remove(&refresh_expiry_key(account_id))?;
            return Err(OAuthError::WaitingForAuthorization);
        }
        let refresh_token = self
            .credentials
            .get(&key)?
            .ok_or(OAuthError::WaitingForAuthorization)?;
        let token: TokenResponse = match self.post_form(
            &self.endpoints.token,
            "refresh token exchange",
            &[
                ("client_id", &self.client_id),
                ("refresh_token", &refresh_token),
                ("grant_type", "refresh_token"),
            ],
        ) {
            Ok(token) => token,
            Err(OAuthError::InvalidGrant) => {
                self.credentials.remove(&key)?;
                self.credentials.remove(&refresh_expiry_key(account_id))?;
                *self
                    .cached
                    .lock()
                    .map_err(|_| OAuthError::SecureStoreFailure)? = None;
                return Err(OAuthError::WaitingForAuthorization);
            }
            Err(error) => return Err(error),
        };
        if token.access_token.is_empty() || token.expires_in == 0 {
            return Err(OAuthError::InvalidResponse);
        }
        if let Some(refresh_token) = token.refresh_token.as_deref() {
            self.credentials.put(&key, refresh_token)?;
        }
        if let Some(lifetime) = token.refresh_token_expires_in {
            self.save_refresh_expiry(account_id, lifetime)?;
        }
        *self
            .cached
            .lock()
            .map_err(|_| OAuthError::SecureStoreFailure)? = Some(cached_token(
            account_id,
            &token.access_token,
            token.expires_in,
        )?);
        Ok(token.access_token)
    }

    pub fn force_refresh_token(&self, account_id: &str) -> Result<(), OAuthError> {
        *self
            .cached
            .lock()
            .map_err(|_| OAuthError::SecureStoreFailure)? = None;
        self.access_token(account_id).map(|_| ())
    }

    /// Removes only this account's refresh token and in-memory access token.
    pub fn revoke_local_credentials(&self, account_id: &str) -> Result<(), OAuthError> {
        self.credentials.remove(&refresh_key(account_id))?;
        self.credentials.remove(&refresh_expiry_key(account_id))?;
        *self
            .cached
            .lock()
            .map_err(|_| OAuthError::SecureStoreFailure)? = None;
        Ok(())
    }

    fn save_refresh_expiry(
        &self,
        account_id: &str,
        lifetime_seconds: u64,
    ) -> Result<(), OAuthError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| OAuthError::InvalidSystemTime)?
            .as_secs();
        self.credentials.put(
            &refresh_expiry_key(account_id),
            &now.saturating_add(lifetime_seconds).to_string(),
        )
    }

    fn refresh_token_expired(&self, account_id: &str) -> Result<bool, OAuthError> {
        let Some(value) = self.credentials.get(&refresh_expiry_key(account_id))? else {
            return Ok(false);
        };
        let expiry = value
            .parse::<u64>()
            .map_err(|_| OAuthError::SecureStoreFailure)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| OAuthError::InvalidSystemTime)?
            .as_secs();
        Ok(expiry <= now)
    }

    fn post_form<T: for<'de> Deserialize<'de>>(
        &self,
        endpoint: &str,
        operation: &'static str,
        fields: &[(&str, &str)],
    ) -> Result<T, OAuthError> {
        let response = match self.agent.post(endpoint).send_form(fields) {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let (code, description) = provider_error(response);
                if operation == "refresh token exchange" && status == 400 && code == "invalid_grant"
                {
                    return Err(OAuthError::InvalidGrant);
                }
                return Err(OAuthError::ProviderFailure {
                    operation,
                    status,
                    code,
                    description,
                });
            }
            Err(ureq::Error::Transport(_)) => return Err(OAuthError::Network),
        };
        response
            .into_json()
            .map_err(|_| OAuthError::InvalidResponse)
    }

    fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        endpoint: &str,
        access_token: &str,
    ) -> Result<T, OAuthError> {
        let response = self
            .agent
            .get(endpoint)
            .set("Authorization", &format!("Bearer {access_token}"))
            .call()
            .map_err(|error| map_ureq_error(error, "account identity lookup"))?;
        response
            .into_json()
            .map_err(|_| OAuthError::InvalidResponse)
    }
}

#[cfg(test)]
pub(crate) fn mock_oauth_client(
    account_id: &str,
    access_token: &str,
    credentials: Arc<dyn CredentialStore>,
) -> Arc<OAuthClient> {
    Arc::new(OAuthClient {
        client_id: "mock-desktop-client".to_owned(),
        endpoints: OAuthEndpoints::default(),
        credentials,
        agent: ureq::AgentBuilder::new().build(),
        cached: Mutex::new(Some(CachedToken {
            account_id: account_id.to_owned(),
            access_token: access_token.to_owned(),
            expires_at: Instant::now() + Duration::from_secs(3600),
        })),
    })
}

fn cached_token(
    account_id: &str,
    access_token: &str,
    lifetime_seconds: u64,
) -> Result<CachedToken, OAuthError> {
    if lifetime_seconds == 0 || lifetime_seconds > 31_536_000 {
        return Err(OAuthError::InvalidResponse);
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| OAuthError::InvalidSystemTime)?;
    Ok(CachedToken {
        account_id: account_id.to_owned(),
        access_token: access_token.to_owned(),
        expires_at: Instant::now() + Duration::from_secs(lifetime_seconds),
    })
}

fn refresh_key(account_id: &str) -> String {
    format!("refresh:{account_id}")
}

fn refresh_expiry_key(account_id: &str) -> String {
    format!("refresh-expiry:{account_id}")
}

fn map_ureq_error(error: ureq::Error, operation: &'static str) -> OAuthError {
    match error {
        ureq::Error::Status(status, response) => {
            let (code, description) = provider_error(response);
            OAuthError::ProviderFailure {
                operation,
                status,
                code,
                description,
            }
        }
        ureq::Error::Transport(_) => OAuthError::Network,
    }
}

fn provider_error(response: ureq::Response) -> (String, String) {
    #[derive(Deserialize)]
    struct ErrorResponse {
        error: Option<String>,
        error_description: Option<String>,
    }

    let Ok(error) = response.into_json::<ErrorResponse>() else {
        return (
            "unknown".to_owned(),
            "no error description supplied".to_owned(),
        );
    };
    let code = error
        .error
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 64
                && value
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "_.-".contains(character))
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let description = error
        .error_description
        .map(|value| {
            value
                .chars()
                .map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                })
                .take(160)
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "no error description supplied".to_owned());
    (code, description)
}

fn validate_google_endpoint(
    value: &str,
    expected_host: &str,
    expected_path: &str,
) -> Result<Url, OAuthError> {
    let url = Url::parse(value).map_err(|_| OAuthError::InvalidConfiguration)?;
    if url.scheme() != "https"
        || url.host_str() != Some(expected_host)
        || url.path() != expected_path
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.username() != ""
        || url.password().is_some()
    {
        return Err(OAuthError::InvalidConfiguration);
    }
    Ok(url)
}

fn read_callback(stream: &mut TcpStream) -> Result<String, OAuthError> {
    let mut request = Vec::with_capacity(1024);
    let mut byte = [0_u8; 1];
    while request.len() < MAX_CALLBACK_BYTES {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => return Err(OAuthError::Network),
        }
    }
    if request.len() >= MAX_CALLBACK_BYTES || !request.ends_with(b"\r\n\r\n") {
        return Err(OAuthError::AuthorizationDenied);
    }
    let line = std::str::from_utf8(&request)
        .map_err(|_| OAuthError::AuthorizationDenied)?
        .lines()
        .next()
        .ok_or(OAuthError::AuthorizationDenied)?;
    let mut words = line.split_ascii_whitespace();
    if words.next() != Some("GET") {
        return Err(OAuthError::AuthorizationDenied);
    }
    let target = words.next().ok_or(OAuthError::AuthorizationDenied)?;
    if !target.starts_with('/') || target.starts_with("//") {
        return Err(OAuthError::AuthorizationDenied);
    }
    Ok(target.to_owned())
}

fn write_callback_response(stream: &mut TcpStream, accepted: bool) {
    let body = if accepted {
        "Authorization received. You can close this tab and return to RustBackup."
    } else {
        "Authorization could not be completed. Return to RustBackup and try again."
    };
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        if accepted { "200 OK" } else { "400 Bad Request" },
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    use super::*;

    #[test]
    fn oauth_endpoints_reject_explicit_ports_and_unexpected_hosts() {
        assert!(validate_google_endpoint(
            "https://accounts.google.com:8443/o/oauth2/v2/auth",
            "accounts.google.com",
            "/o/oauth2/v2/auth"
        )
        .is_err());
        assert!(validate_google_endpoint(
            "https://accounts.google.com.attacker.test/o/oauth2/v2/auth",
            "accounts.google.com",
            "/o/oauth2/v2/auth"
        )
        .is_err());
    }

    #[derive(Default)]
    struct MemoryCredentials(Mutex<HashMap<String, String>>);

    impl CredentialStore for MemoryCredentials {
        fn get(&self, key: &str) -> Result<Option<String>, OAuthError> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }

        fn put(&self, key: &str, value: &str) -> Result<(), OAuthError> {
            self.0
                .lock()
                .unwrap()
                .insert(key.to_owned(), value.to_owned());
            Ok(())
        }

        fn remove(&self, key: &str) -> Result<(), OAuthError> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }
    }

    #[test]
    fn authorization_url_uses_s256_pkce_and_restricted_scope() {
        let (authorization_url, session) =
            begin_authorization("desktop-client-id", &OAuthEndpoints::default()).unwrap();
        let authorization_url = Url::parse(&authorization_url).unwrap();
        let values = authorization_url.query_pairs().collect::<HashMap<_, _>>();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(session.verifier.as_bytes()));
        assert_eq!(values.get("response_type").unwrap(), "code");
        assert_eq!(values.get("code_challenge_method").unwrap(), "S256");
        assert_eq!(values.get("code_challenge").unwrap(), challenge.as_str());
        assert!(values.get("scope").unwrap().contains("drive.file"));
        assert_eq!(values.get("scope").unwrap().split_whitespace().count(), 3);
        assert_eq!(values.get("state").unwrap(), &session.state);
        assert!(session.redirect_uri.starts_with("http://127.0.0.1:"));
        assert!(session.verifier.len() >= 43 && session.verifier.len() <= 128);
    }

    #[test]
    fn loopback_callback_checks_state_and_is_single_use() {
        let (_, session) = begin_authorization("client", &OAuthEndpoints::default()).unwrap();
        let address = session.listener.local_addr().unwrap();
        let expected_state = session.state.clone();
        let callback = thread::spawn(move || session.wait_for_code(Duration::from_secs(3)));
        send_callback(address, &expected_state, "authorization-code");
        let authorization = callback.join().unwrap().unwrap();
        assert_eq!(authorization.code, "authorization-code");
        assert_eq!(authorization.verifier.len(), 86);

        let (_, session) = begin_authorization("client", &OAuthEndpoints::default()).unwrap();
        let address = session.listener.local_addr().unwrap();
        let callback = thread::spawn(move || session.wait_for_code(Duration::from_secs(3)));
        let response = send_callback(address, "incorrect-state", "authorization-code");
        assert!(response.contains("400 Bad Request"));
        assert!(matches!(
            callback.join().unwrap(),
            Err(OAuthError::AuthorizationDenied)
        ));
    }

    #[test]
    fn authorization_errors_show_safe_provider_description_and_failing_stage() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            assert!(request.starts_with("POST /token "));
            write_http_response(
                &mut stream,
                400,
                "application/json",
                r#"{"error":"invalid_request","error_description":"Missing required parameter: code_verifier.\nRetry the request."}"#,
            );
        });
        let client = OAuthClient {
            client_id: "desktop-client".to_owned(),
            endpoints: OAuthEndpoints {
                authorization: format!("{endpoint}/auth"),
                token: format!("{endpoint}/token"),
                userinfo: format!("{endpoint}/userinfo"),
            },
            credentials: Arc::new(MemoryCredentials::default()),
            agent: ureq::AgentBuilder::new().build(),
            cached: Mutex::new(None),
        };

        let error = client
            .complete_authorization(AuthorizationCode {
                code: "one-time-code".to_owned(),
                redirect_uri: "http://127.0.0.1:43210".to_owned(),
                verifier: "v".repeat(64),
            })
            .expect_err("provider rejection should be surfaced");

        assert_eq!(
            error.to_string(),
            "Google authorization code exchange failed (HTTP 400; provider error: invalid_request; Missing required parameter: code_verifier. Retry the request.)"
        );
        server.join().unwrap();
    }

    #[test]
    fn authorization_saves_only_refresh_token_and_refreshes_access_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                match index {
                    0 => {
                        assert!(request.starts_with("POST /token "));
                        assert!(request.contains("code_verifier="));
                        write_http_response(
                            &mut stream,
                            200,
                            "application/json",
                            r#"{"access_token":"access-one","expires_in":3600,"refresh_token":"refresh-secret"}"#,
                        );
                    }
                    1 => {
                        assert!(request.starts_with("GET /userinfo "));
                        assert!(request
                            .to_ascii_lowercase()
                            .contains("authorization: bearer access-one"));
                        write_http_response(
                            &mut stream,
                            200,
                            "application/json",
                            r#"{"sub":"account-123","email":"user@example.test"}"#,
                        );
                    }
                    _ => {
                        assert!(request.starts_with("POST /token "));
                        assert!(request.contains("grant_type=refresh_token"));
                        assert!(request.contains("refresh_token=refresh-secret"));
                        write_http_response(
                            &mut stream,
                            200,
                            "application/json",
                            r#"{"access_token":"access-two","expires_in":3600}"#,
                        );
                    }
                }
            }
        });
        let credentials = Arc::new(MemoryCredentials::default());
        let client = OAuthClient {
            client_id: "desktop-client".to_owned(),
            endpoints: OAuthEndpoints {
                authorization: format!("{endpoint}/auth"),
                token: format!("{endpoint}/token"),
                userinfo: format!("{endpoint}/userinfo"),
            },
            credentials: credentials.clone(),
            agent: ureq::AgentBuilder::new().build(),
            cached: Mutex::new(None),
        };
        let identity = client
            .complete_authorization(AuthorizationCode {
                code: "one-time-code".to_owned(),
                redirect_uri: "http://127.0.0.1:43210/oauth2callback".to_owned(),
                verifier: "v".repeat(64),
            })
            .unwrap();
        assert_eq!(identity.subject, "account-123");
        assert_eq!(identity.email.as_deref(), Some("user@example.test"));
        assert_eq!(
            credentials.get("refresh:account-123").unwrap().as_deref(),
            Some("refresh-secret")
        );
        assert!(credentials.get("access:account-123").unwrap().is_none());
        assert_eq!(client.access_token("account-123").unwrap(), "access-one");
        client.force_refresh_token("account-123").unwrap();
        assert_eq!(client.access_token("account-123").unwrap(), "access-two");
        assert_eq!(
            credentials.get("refresh:account-123").unwrap().as_deref(),
            Some("refresh-secret")
        );
        server.join().unwrap();
    }

    fn send_callback(address: std::net::SocketAddr, state: &str, code: &str) -> String {
        let mut stream = TcpStream::connect(address).unwrap();
        write!(
            stream,
            "GET /?state={state}&code={code} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        let header_end = request.len();
        let headers = String::from_utf8_lossy(&request).to_ascii_lowercase();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        request.resize(header_end + content_length, 0);
        stream.read_exact(&mut request[header_end..]).unwrap();
        String::from_utf8_lossy(&request).into_owned()
    }

    fn write_http_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
        let reason = if status == 200 { "OK" } else { "Error" };
        write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    }
}
