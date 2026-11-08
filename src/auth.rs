//! Sign in with ChatGPT: dynamic-registration OAuth with PKCE over a loopback redirect,
//! RS256 id_token checks, refresh, revoke, and Keychain storage.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::signature::Verifier;
use rsa::{BigUint, RsaPublicKey};
use security_framework::passwords::{
    PasswordOptions, delete_generic_password, generic_password, set_generic_password,
};
use security_framework::random::SecRandom;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use url::Url;

// ponytail: endpoints are hardcoded, verified live 2026-11-08 against
// https://auth.openai.com/.well-known/openid-configuration. Load discovery at runtime if they move.
const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const REVOKE_URL: &str = "https://auth.openai.com/api/accounts/oauth/revoke";
const JWKS_URL: &str = "https://auth.openai.com/.well-known/jwks.json";
const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const KEYCHAIN_SERVICE: &str = "io.github.syrymuly.patina";
const KEYCHAIN_ACCOUNT: &str = "credentials";
const REFRESH_MARGIN_SECS: i64 = 120;
/// Documented access-token lifetime, used if a refresh response leaves out `expires_in`.
const DEFAULT_LIFETIME_SECS: i64 = 3600;
const CLOCK_SKEW_SECS: i64 = 5;
const TERMINAL_REFRESH_ERRORS: &[&str] = &[
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub email: String,
    pub name: Option<String>,
    pub plan_usage: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthError {
    Cancelled,
    Denied,
    SignInRequired,
    Network(String),
    Protocol(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AuthError::Cancelled => f.write_str("Sign-in was cancelled."),
            AuthError::Denied => f.write_str("Access was not allowed in ChatGPT."),
            AuthError::SignInRequired => f.write_str("Sign in with ChatGPT to continue."),
            AuthError::Network(m) => write!(f, "Could not reach ChatGPT: {m}"),
            AuthError::Protocol(m) => write!(f, "Sign-in failed: {m}"),
        }
    }
}

impl std::error::Error for AuthError {}

fn network(e: impl fmt::Display) -> AuthError {
    AuthError::Network(e.to_string())
}

fn protocol(e: impl fmt::Display) -> AuthError {
    AuthError::Protocol(e.to_string())
}

/// Non-secret registration in data_dir/account.json. Survives sign-out.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Registration {
    ext_agent_host_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
}

/// Secrets, stored in the Keychain only. No Debug on purpose.
#[derive(Clone, Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_at: i64,
    scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    email: String,
}

impl Tokens {
    fn account(&self) -> Account {
        Account {
            email: self.email.clone(),
            name: self.name.clone(),
            plan_usage: plan_usage(&self.scopes),
        }
    }
}

struct State {
    reg: Registration,
    tokens: Option<Tokens>,
}

pub struct Auth {
    dir: PathBuf,
    /// False in tests so they never touch the Keychain.
    keychain: bool,
    http: ureq::Agent,
    state: Mutex<State>,
    /// Held across a whole refresh or revoke so they never overlap.
    refresh: Mutex<()>,
    jwks: Mutex<Vec<Jwk>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Auth {
    /// Loads saved registration (data_dir/account.json) and tokens (Keychain). Never fails: missing = signed out.
    pub fn load(data_dir: &Path) -> Auth {
        Auth::new(data_dir, true)
    }

    fn new(dir: &Path, keychain: bool) -> Auth {
        let mut reg: Registration = fs::read(dir.join("account.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if reg.ext_agent_host_id.is_empty() {
            reg.ext_agent_host_id = uuid::Builder::from_random_bytes(random())
                .into_uuid()
                .urn()
                .to_string();
        }
        let registered = reg.client_id.is_some();
        let auth = Auth {
            dir: dir.to_path_buf(),
            keychain,
            http: agent(),
            state: Mutex::new(State { reg, tokens: None }),
            refresh: Mutex::default(),
            jwks: Mutex::default(),
        };
        // Tokens without a registration can be neither refreshed nor revoked.
        if registered {
            lock(&auth.state).tokens = auth.keychain_load();
        }
        auth
    }

    pub fn account(&self) -> Option<Account> {
        lock(&self.state).tokens.as_ref().map(Tokens::account)
    }

    /// Blocking. Opens the browser through `open_url`, waits for the loopback callback (gives up
    /// after 10 minutes or when `cancel` is set), exchanges the code, verifies the id_token and saves.
    pub fn sign_in(
        &self,
        app_name: &str,
        cancel: &AtomicBool,
        open_url: impl FnOnce(&str),
    ) -> Result<Account, AuthError> {
        let reg = lock(&self.state).reg.clone();
        // Persists ext_agent_host_id before it is ever sent.
        self.save_registration(&reg)?;
        let listener = TcpListener::bind("127.0.0.1:0").map_err(network)?;
        let port = listener.local_addr().map_err(network)?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");
        let verifier = B64.encode(random::<32>());
        let state = B64.encode(random::<32>());
        let nonce = B64.encode(random::<32>());
        let challenge = pkce_challenge(&verifier);
        open_url(&authorize_url(
            &reg,
            app_name,
            &redirect_uri,
            &state,
            &nonce,
            &challenge,
        ));
        let (stream, callback) = wait_for_callback(
            &listener,
            &state,
            reg.client_id.as_deref(),
            app_name,
            cancel,
        )?;
        drop(listener);
        let result = self.finish_sign_in(reg, callback, &verifier, &redirect_uri, &nonce);
        match &result {
            Ok(_) => respond(
                &stream,
                "200 OK",
                &format!("You're signed in. You can close this tab and return to {app_name}."),
            ),
            Err(_) => respond(
                &stream,
                "400 Bad Request",
                &format!("Sign-in did not finish. Return to {app_name} and try again."),
            ),
        }
        result
    }

    fn finish_sign_in(
        &self,
        mut reg: Registration,
        callback: Callback,
        verifier: &str,
        redirect_uri: &str,
        nonce: &str,
    ) -> Result<Account, AuthError> {
        let client_id = callback.client_id;
        if reg.client_id.is_none() {
            // Keep the issued id even if the exchange fails, so a retry reuses the registration.
            reg.client_id = Some(client_id.clone());
            self.save_registration(&reg)?;
            lock(&self.state).reg = reg.clone();
        }
        let (status, body) = self.post_form(
            TOKEN_URL,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", &client_id),
                ("code", &callback.code),
                ("code_verifier", verifier),
                ("redirect_uri", redirect_uri),
                ("resource", RESOURCE),
            ],
        )?;
        if status != 200 {
            return Err(endpoint_error(status, &body));
        }
        let grant = parse_token_response(&body, now(), None)?;
        let (Some(id_token), Some(refresh_token)) = (grant.id_token, grant.refresh_token) else {
            return Err(protocol("the token response has no ID or refresh token."));
        };
        let claims: Claims = serde_json::from_slice(&self.verified_payload(&id_token)?)
            .map_err(|e| protocol(format!("the ID token is invalid ({e}).")))?;
        check_claims(&claims, &client_id, nonce, now())?;
        if reg.subject.as_ref().is_some_and(|s| *s != claims.sub) {
            // Another account needs its own registration: revoke both grants, start fresh.
            self.revoke(&refresh_token, &client_id);
            let _serial = lock(&self.refresh);
            let previous = lock(&self.state).tokens.take();
            if let Some(previous) = previous {
                self.revoke(&previous.refresh_token, &client_id);
            }
            let fresh = Registration {
                ext_agent_host_id: reg.ext_agent_host_id,
                client_id: None,
                subject: None,
                email: None,
            };
            self.save_registration(&fresh)?;
            let _ = self.keychain_save(None);
            lock(&self.state).reg = fresh;
            return Err(protocol(
                "that is a different ChatGPT account than before. Sign in again to use it.",
            ));
        }
        reg.subject = Some(claims.sub);
        reg.email.clone_from(&claims.email);
        let tokens = Tokens {
            access_token: grant.access_token,
            refresh_token,
            id_token,
            expires_at: grant.expires_at,
            scopes: grant.scopes,
            name: claims.name,
            email: claims.email.unwrap_or_default(),
        };
        let account = tokens.account();
        {
            // Serialized with refresh and sign-out so neither can overwrite the new grant.
            // ponytail: the replaced grant is not revoked, in case the server ties it to the new one.
            let _serial = lock(&self.refresh);
            self.save_registration(&reg)?;
            self.keychain_save(Some(&tokens))?;
            *lock(&self.state) = State {
                reg,
                tokens: Some(tokens),
            };
        }
        Ok(account)
    }

    /// Returns a valid access token, refreshing (serialized) when it expires within 120 s.
    /// Terminal refresh errors clear tokens and return SignInRequired.
    pub fn access_token(&self) -> Result<String, AuthError> {
        let _serial = lock(&self.refresh);
        let (client_id, old) = {
            let state = lock(&self.state);
            let tokens = state.tokens.as_ref().ok_or(AuthError::SignInRequired)?;
            if tokens.expires_at - now() > REFRESH_MARGIN_SECS {
                return Ok(tokens.access_token.clone());
            }
            let client_id = state.reg.client_id.clone();
            (client_id.ok_or(AuthError::SignInRequired)?, tokens.clone())
        };
        let refreshed = self.post_form(
            TOKEN_URL,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &client_id),
                ("refresh_token", &old.refresh_token),
                ("resource", RESOURCE),
            ],
        );
        // A temporary failure is not fatal while the current token still has a little life left.
        let still_valid = |e: AuthError| {
            if old.expires_at - now() > 5 {
                Ok(old.access_token.clone())
            } else {
                Err(e)
            }
        };
        let (status, body) = match refreshed {
            Ok(r) => r,
            Err(e) => return still_valid(e),
        };
        if status != 200 {
            if refresh_is_terminal(status, &body) {
                lock(&self.state).tokens = None;
                let _ = self.keychain_save(None);
                return Err(AuthError::SignInRequired);
            }
            return still_valid(endpoint_error(status, &body));
        }
        let grant = parse_token_response(&body, now(), Some(&old.scopes))?;
        // ponytail: a refreshed id_token is stored, not re-verified; name and email come from sign-in.
        let tokens = Tokens {
            access_token: grant.access_token,
            refresh_token: grant.refresh_token.unwrap_or(old.refresh_token),
            id_token: grant.id_token.unwrap_or(old.id_token),
            expires_at: grant.expires_at,
            scopes: grant.scopes,
            ..old
        };
        // Saved right away since the old refresh token is now spent. If the Keychain write
        // fails, the in-memory copy still works for this session.
        let _ = self.keychain_save(Some(&tokens));
        let access_token = tokens.access_token.clone();
        lock(&self.state).tokens = Some(tokens);
        Ok(access_token)
    }

    /// Revokes the refresh token (best effort, 2 tries), clears tokens, keeps client_id + host id.
    /// Returns whether revocation was confirmed.
    pub fn sign_out(&self) -> bool {
        let _serial = lock(&self.refresh);
        let (tokens, client_id) = {
            let mut state = lock(&self.state);
            (state.tokens.take(), state.reg.client_id.clone())
        };
        let confirmed = match tokens.zip(client_id) {
            Some((tokens, client_id)) => self.revoke(&tokens.refresh_token, &client_id),
            None => true,
        };
        let _ = self.keychain_save(None);
        confirmed
    }

    fn revoke(&self, refresh_token: &str, client_id: &str) -> bool {
        for attempt in 0..2 {
            if attempt > 0 {
                thread::sleep(Duration::from_millis(300));
            }
            let form = [
                ("token", refresh_token),
                ("token_type_hint", "refresh_token"),
                ("client_id", client_id),
            ];
            match self.post_form(REVOKE_URL, &form) {
                Ok((200, _)) => return true,
                Ok((status, _)) if status < 500 => return false,
                _ => {}
            }
        }
        false
    }

    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, String), AuthError> {
        let mut response = self
            .http
            .post(url)
            .header("Accept", "application/json")
            .send_form(form.iter().copied())
            .map_err(network)?;
        let body = response.body_mut().read_to_string().map_err(network)?;
        Ok((response.status().as_u16(), body))
    }

    /// Verifies the id_token signature against the cached JWKS, refetching once for an unknown kid.
    fn verified_payload(&self, jwt: &str) -> Result<Vec<u8>, AuthError> {
        let kid = jwt_header(jwt)?.kid;
        let mut keys = lock(&self.jwks);
        if !keys.iter().any(|k| k.kid == kid) {
            *keys = self.fetch_jwks()?;
        }
        let key = keys
            .iter()
            .find(|k| k.kid == kid)
            .ok_or_else(|| protocol("the ID token was signed with an unknown key."))?;
        verify_rs256(jwt, key)
    }

    fn fetch_jwks(&self) -> Result<Vec<Jwk>, AuthError> {
        #[derive(Deserialize)]
        struct Jwks {
            keys: Vec<Jwk>,
        }
        let mut response = self
            .http
            .get(JWKS_URL)
            .header("Accept", "application/json")
            .call()
            .map_err(network)?;
        if response.status() != 200 {
            return Err(network(format!(
                "signing keys unavailable (HTTP {}).",
                response.status().as_u16()
            )));
        }
        let body = response.body_mut().read_to_string().map_err(network)?;
        let jwks: Jwks = serde_json::from_str(&body).map_err(protocol)?;
        Ok(jwks.keys)
    }

    fn save_registration(&self, reg: &Registration) -> Result<(), AuthError> {
        let write = || -> io::Result<()> {
            fs::create_dir_all(&self.dir)?;
            let tmp = self.dir.join("account.json.tmp");
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(&serde_json::to_vec_pretty(reg)?)?;
            file.sync_all()?;
            fs::rename(&tmp, self.dir.join("account.json"))
        };
        write().map_err(|e| protocol(format!("could not save the sign-in ({e}).")))
    }

    fn keychain_load(&self) -> Option<Tokens> {
        if !self.keychain {
            return None;
        }
        let options = PasswordOptions::new_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
        serde_json::from_slice(&generic_password(options).ok()?).ok()
    }

    /// `None` deletes the item.
    fn keychain_save(&self, tokens: Option<&Tokens>) -> Result<(), AuthError> {
        if !self.keychain {
            return Ok(());
        }
        let Some(tokens) = tokens else {
            // A missing item is already the desired state.
            let _ = delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
            return Ok(());
        };
        let json = serde_json::to_vec(tokens).map_err(protocol)?;
        set_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, &json)
            .map_err(|e| protocol(format!("could not save to the Keychain ({e}).")))
    }
}

fn agent() -> ureq::Agent {
    let tls = TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    SecRandom::default()
        .copy_bytes(&mut bytes)
        .expect("system random generator failed");
    bytes
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn pkce_challenge(verifier: &str) -> String {
    B64.encode(Sha256::digest(verifier))
}

fn plan_usage(scopes: &[String]) -> bool {
    scopes.iter().any(|s| s == PLAN_SCOPE)
}

fn authorize_url(
    reg: &Registration,
    app_name: &str,
    redirect_uri: &str,
    state: &str,
    nonce: &str,
    challenge: &str,
) -> String {
    let mut params = vec![
        ("response_type", "code"),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPE),
        ("resource", RESOURCE),
        ("state", state),
        ("nonce", nonce),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("ext_agent_host_id", &reg.ext_agent_host_id),
    ];
    match &reg.client_id {
        None => params.extend([("client_id", DYNAMIC_CLIENT), ("agent_name_hint", app_name)]),
        Some(client_id) => {
            params.push(("client_id", client_id));
            if let Some(email) = &reg.email {
                params.push(("login_hint", email));
            }
        }
    }
    Url::parse_with_params(AUTHORIZE_URL, params)
        .expect("authorize URL is valid")
        .into()
}

struct Callback {
    code: String,
    client_id: String,
}

/// Serves the loopback redirect until a valid callback arrives, `cancel` is set or 10 minutes pass.
/// Returns the connection on success so the caller can answer once the exchange is done.
fn wait_for_callback(
    listener: &TcpListener,
    state: &str,
    saved_client_id: Option<&str>,
    app_name: &str,
    cancel: &AtomicBool,
) -> Result<(TcpStream, Callback), AuthError> {
    listener.set_nonblocking(true).map_err(network)?;
    let deadline = Instant::now() + Duration::from_secs(600);
    while !cancel.load(Ordering::Relaxed) && Instant::now() < deadline {
        let Ok((stream, _)) = listener.accept() else {
            thread::sleep(Duration::from_millis(100));
            continue;
        };
        let Some(target) = read_request_target(&stream) else {
            continue;
        };
        match parse_callback(&target, state, saved_client_id) {
            None => respond(
                &stream,
                "400 Bad Request",
                "This is not a valid sign-in response.",
            ),
            Some(Ok(callback)) => return Ok((stream, callback)),
            Some(Err(e)) => {
                let line = match e {
                    AuthError::Denied => format!(
                        "Sign-in was cancelled. You can close this tab and return to {app_name}."
                    ),
                    _ => format!("Sign-in did not finish. Return to {app_name} and try again."),
                };
                respond(&stream, "400 Bad Request", &line);
                return Err(e);
            }
        }
    }
    Err(AuthError::Cancelled)
}

/// Reads one HTTP request (line and headers) and returns the GET target.
fn read_request_target(stream: &TcpStream) -> Option<String> {
    stream.set_nonblocking(false).ok()?;
    // One budget for the whole request, so a slow client can't stall the cancel check.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut reader = BufReader::new(stream.take(16 * 1024));
    let mut read_line = |buf: &mut String| {
        let left = deadline.saturating_duration_since(Instant::now());
        !left.is_zero()
            && stream.set_read_timeout(Some(left)).is_ok()
            && reader.read_line(buf).is_ok_and(|n| n > 0)
    };
    let mut line = String::new();
    if !read_line(&mut line) {
        return None;
    }
    let target = line.strip_prefix("GET ")?.split(' ').next()?.to_string();
    // Drain the headers: closing with unread input resets the connection before the page shows.
    let mut header = String::new();
    while read_line(&mut header) && header.len() > 2 {
        header.clear();
    }
    Some(target)
}

fn respond(mut stream: &TcpStream, status: &str, text: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><meta name=\"color-scheme\" content=\"light dark\">\
         <title>Sign in</title><p style=\"font: 17px/1.5 system-ui, sans-serif; \
         max-width: 32em; margin: 20vh auto; padding: 0 24px\">{text}</p>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// `None` means not our redirect (wrong path or state): answer 400 and keep waiting.
fn parse_callback(
    target: &str,
    state: &str,
    saved_client_id: Option<&str>,
) -> Option<Result<Callback, AuthError>> {
    let url = Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    if url.path() != "/auth/callback" {
        return None;
    }
    let all = |key: &str| -> Vec<String> {
        url.query_pairs()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
            .collect()
    };
    match all("state").as_slice() {
        [got] if ct_eq(got.as_bytes(), state.as_bytes()) => {}
        _ => return None,
    }
    let first = |key: &str| all(key).into_iter().next();
    if let Some(error) = first("error") {
        return Some(Err(if error == "access_denied" {
            AuthError::Denied
        } else {
            protocol(format!("ChatGPT returned {error}."))
        }));
    }
    let codes = all("code");
    let [code] = codes.as_slice() else {
        return Some(Err(protocol("expected exactly one authorization code.")));
    };
    let client_id = match (first("client_id"), saved_client_id) {
        (Some(got), Some(saved)) if got != saved => {
            return Some(Err(protocol(
                "ChatGPT returned a different app registration.",
            )));
        }
        (Some(got), _) => got,
        (None, Some(saved)) => saved.to_string(),
        (None, None) => return Some(Err(protocol("ChatGPT did not return an app registration."))),
    };
    let well_formed = (1..=200).contains(&client_id.len())
        && client_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !well_formed || client_id == DYNAMIC_CLIENT {
        return Some(Err(protocol(
            "ChatGPT returned an invalid app registration.",
        )));
    }
    Some(Ok(Callback {
        code: code.clone(),
        client_id,
    }))
}

struct Grant {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_at: i64,
    scopes: Vec<String>,
}

/// `previous_scopes` is set on refresh, where the server may omit `scope`.
fn parse_token_response(
    body: &str,
    now: i64,
    previous_scopes: Option<&[String]>,
) -> Result<Grant, AuthError> {
    #[derive(Deserialize)]
    struct Raw {
        access_token: String,
        refresh_token: Option<String>,
        id_token: Option<String>,
        token_type: Option<String>,
        expires_in: Option<i64>,
        scope: Option<String>,
    }
    let raw: Raw = serde_json::from_str(body)
        .map_err(|e| protocol(format!("unexpected token response ({e}).")))?;
    // A refresh is accepted leniently: its rotated refresh token must not be thrown away
    // over a missing field, or the next refresh fails and the user is signed out.
    let refreshing = previous_scopes.is_some();
    let token_type = raw
        .token_type
        .unwrap_or_else(|| if refreshing { "Bearer" } else { "" }.into());
    let expires_in = raw
        .expires_in
        .unwrap_or(if refreshing { DEFAULT_LIFETIME_SECS } else { 0 });
    if !token_type.eq_ignore_ascii_case("bearer") || expires_in <= 0 {
        return Err(protocol("unexpected token type or lifetime."));
    }
    let scopes = match (raw.scope, previous_scopes) {
        (Some(scope), _) => scope.split_whitespace().map(String::from).collect(),
        (None, Some(previous)) => previous.to_vec(),
        (None, None) => return Err(protocol("the token response has no granted scope.")),
    };
    Ok(Grant {
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        id_token: raw.id_token,
        expires_at: now.saturating_add(expires_in),
        scopes,
    })
}

/// The error code of an OAuth error body (`{"error":"c"}`, `{"error":{"code":"c"}}` or
/// `{"code":"c"}`), or "" when there is none.
fn oauth_error(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            [&v["error"], &v["error"]["code"], &v["code"]]
                .into_iter()
                .find_map(|c| c.as_str().map(String::from))
        })
        .unwrap_or_default()
}

/// A dead refresh token: drop the credentials and ask the user to sign in again.
fn refresh_is_terminal(status: u16, body: &str) -> bool {
    status == 401 || TERMINAL_REFRESH_ERRORS.contains(&oauth_error(body).as_str())
}

fn endpoint_error(status: u16, body: &str) -> AuthError {
    if status >= 500 || status == 429 {
        return network(format!(
            "the sign-in service is unavailable (HTTP {status})."
        ));
    }
    match oauth_error(body) {
        code if code.is_empty() => protocol(format!("the request was rejected (HTTP {status}).")),
        code => protocol(format!("the request was rejected ({code}, HTTP {status}).")),
    }
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    aud: Audience,
    azp: Option<String>,
    exp: i64,
    /// Required; presence is the check.
    #[serde(rename = "iat")]
    _issued_at: i64,
    nonce: Option<String>,
    sub: String,
    email: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

fn check_claims(c: &Claims, client_id: &str, nonce: &str, now: i64) -> Result<(), AuthError> {
    let audience = match &c.aud {
        Audience::One(aud) => std::slice::from_ref(aud),
        Audience::Many(aud) => aud.as_slice(),
    };
    let for_us = audience.iter().any(|a| a == client_id)
        && (audience.len() == 1 || c.azp.is_some())
        && c.azp.as_deref().is_none_or(|azp| azp == client_id);
    let problem = if c.iss != ISSUER {
        "the ID token has an unexpected issuer."
    } else if !for_us {
        "the ID token is not for this app."
    } else if now > c.exp.saturating_add(CLOCK_SKEW_SECS) {
        "the ID token has expired."
    } else if c.nonce.as_deref() != Some(nonce) {
        "the ID token does not belong to this sign-in."
    } else if c.sub.is_empty() {
        "the ID token has no subject."
    } else {
        return Ok(());
    };
    Err(protocol(problem))
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Jwk {
    kid: String,
    kty: String,
    n: String,
    e: String,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    kid: String,
}

fn jwt_header(jwt: &str) -> Result<Header, AuthError> {
    let part = jwt.split('.').next().unwrap_or_default();
    B64.decode(part)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| protocol("the ID token is malformed."))
}

/// Verifies an RS256 JWS against `key` and returns the decoded payload.
fn verify_rs256(jwt: &str, key: &Jwk) -> Result<Vec<u8>, AuthError> {
    let bad = || protocol("the ID token signature is invalid.");
    let (signed, signature) = jwt.rsplit_once('.').ok_or_else(bad)?;
    let (_, payload) = signed.split_once('.').ok_or_else(bad)?;
    if jwt_header(jwt)?.alg != "RS256" || key.kty != "RSA" {
        return Err(bad());
    }
    let decode = |s: &str| B64.decode(s).map_err(|_| bad());
    let n = BigUint::from_bytes_be(&decode(&key.n)?);
    let e = BigUint::from_bytes_be(&decode(&key.e)?);
    let public = RsaPublicKey::new(n, e).map_err(|_| bad())?;
    let signature = Signature::try_from(decode(signature)?.as_slice()).map_err(|_| bad())?;
    VerifyingKey::<Sha256>::new(public)
        .verify(signed.as_bytes(), &signature)
        .map_err(|_| bad())?;
    decode(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn query(url: &str) -> HashMap<String, String> {
        Url::parse(url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    fn registration(client_id: Option<&str>, email: Option<&str>) -> Registration {
        Registration {
            ext_agent_host_id: "urn:uuid:00000000-0000-4000-8000-000000000000".into(),
            client_id: client_id.map(String::from),
            subject: None,
            email: email.map(String::from),
        }
    }

    #[test]
    fn pkce_rfc7636_appendix_b() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(B64.encode(random::<32>()).len(), 43);
    }

    #[test]
    fn authorize_url_first_registration() {
        let reg = registration(None, None);
        let url = authorize_url(
            &reg,
            "Patina",
            "http://127.0.0.1:5000/auth/callback",
            "st",
            "no",
            "ch",
        );
        assert!(url.starts_with("https://auth.openai.com/api/accounts/authorize?"));
        let q = query(&url);
        assert_eq!(q["client_id"], "dynamic_agent_client");
        assert_eq!(q["agent_name_hint"], "Patina");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:5000/auth/callback");
        assert_eq!(q["scope"], SCOPE);
        assert_eq!(q["resource"], "https://api.openai.com/v1");
        assert_eq!(q["state"], "st");
        assert_eq!(q["nonce"], "no");
        assert_eq!(q["code_challenge"], "ch");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["ext_agent_host_id"], reg.ext_agent_host_id);
        assert!(!q.contains_key("login_hint"));
    }

    #[test]
    fn authorize_url_returning() {
        let reg = registration(Some("oaiapp_123"), Some("a@b.c"));
        let q = query(&authorize_url(
            &reg,
            "Patina",
            "http://127.0.0.1:1/auth/callback",
            "s",
            "n",
            "c",
        ));
        assert_eq!(q["client_id"], "oaiapp_123");
        assert_eq!(q["login_hint"], "a@b.c");
        assert!(!q.contains_key("agent_name_hint"));
    }

    #[test]
    fn callback_good_first_registration() {
        let cb = parse_callback(
            "/auth/callback?code=abc&state=xyz&client_id=oaiapp_1&scope=openid+email",
            "xyz",
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (cb.code.as_str(), cb.client_id.as_str()),
            ("abc", "oaiapp_1")
        );
    }

    #[test]
    fn callback_returning_keeps_saved_client_id() {
        let cb = parse_callback("/auth/callback?code=abc&state=xyz", "xyz", Some("oaiapp_1"))
            .unwrap()
            .unwrap();
        assert_eq!(cb.client_id, "oaiapp_1");
        let changed = parse_callback(
            "/auth/callback?code=abc&state=xyz&client_id=oaiapp_2",
            "xyz",
            Some("oaiapp_1"),
        );
        assert!(matches!(changed, Some(Err(AuthError::Protocol(_)))));
    }

    #[test]
    fn callback_wrong_state_or_path_keeps_waiting() {
        assert!(
            parse_callback(
                "/auth/callback?code=abc&state=nope&client_id=oaiapp_1",
                "xyz",
                None
            )
            .is_none()
        );
        assert!(
            parse_callback("/auth/callback?code=abc&client_id=oaiapp_1", "xyz", None).is_none()
        );
        assert!(
            parse_callback(
                "/auth/callback?code=a&state=xyz&state=xyz&client_id=x",
                "xyz",
                None
            )
            .is_none()
        );
        assert!(parse_callback("/favicon.ico", "xyz", None).is_none());
        assert!(
            parse_callback("/other?code=abc&state=xyz&client_id=oaiapp_1", "xyz", None).is_none()
        );
    }

    #[test]
    fn callback_access_denied() {
        let r = parse_callback("/auth/callback?error=access_denied&state=xyz", "xyz", None);
        assert!(matches!(r, Some(Err(AuthError::Denied))));
    }

    #[test]
    fn callback_bad_registration() {
        let missing = parse_callback("/auth/callback?code=abc&state=xyz", "xyz", None);
        assert!(matches!(missing, Some(Err(AuthError::Protocol(_)))));
        let dynamic = parse_callback(
            "/auth/callback?code=abc&state=xyz&client_id=dynamic_agent_client",
            "xyz",
            None,
        );
        assert!(matches!(dynamic, Some(Err(AuthError::Protocol(_)))));
        let malformed = parse_callback(
            "/auth/callback?code=abc&state=xyz&client_id=bad%20id",
            "xyz",
            None,
        );
        assert!(matches!(malformed, Some(Err(AuthError::Protocol(_)))));
        let two_codes = parse_callback(
            "/auth/callback?code=a&code=b&state=xyz&client_id=oaiapp_1",
            "xyz",
            None,
        );
        assert!(matches!(two_codes, Some(Err(AuthError::Protocol(_)))));
    }

    fn claims(overrides: serde_json::Value) -> serde_json::Result<Claims> {
        let mut base = json!({
            "iss": "https://auth.openai.com",
            "aud": "oaiapp_1",
            "exp": 1000,
            "iat": 900,
            "nonce": "n1",
            "sub": "user-1",
            "email": "a@b.c",
        });
        for (k, v) in overrides.as_object().unwrap() {
            if v.is_null() {
                base.as_object_mut().unwrap().remove(k);
            } else {
                base[k] = v.clone();
            }
        }
        serde_json::from_value(base)
    }

    fn check(overrides: serde_json::Value, now: i64) -> Result<(), AuthError> {
        check_claims(&claims(overrides).unwrap(), "oaiapp_1", "n1", now)
    }

    #[test]
    fn id_token_claims() {
        assert!(check(json!({}), 950).is_ok());
        assert!(check(json!({}), 1005).is_ok(), "5 s tolerance");
        assert!(check(json!({}), 1006).is_err(), "expired");
        assert!(check(json!({"exp": i64::MAX}), 950).is_ok(), "no overflow");
        assert!(check(json!({"iss": "https://evil.example"}), 950).is_err());
        assert!(check(json!({"aud": "oaiapp_2"}), 950).is_err());
        assert!(check(json!({"aud": ["oaiapp_1"]}), 950).is_ok());
        assert!(
            check(json!({"aud": ["oaiapp_1", "x"]}), 950).is_err(),
            "azp needed"
        );
        assert!(check(json!({"aud": ["oaiapp_1", "x"], "azp": "oaiapp_1"}), 950).is_ok());
        assert!(check(json!({"azp": "oaiapp_2"}), 950).is_err());
        assert!(check(json!({"nonce": "other"}), 950).is_err());
        assert!(check(json!({"nonce": null}), 950).is_err());
        assert!(check(json!({"sub": ""}), 950).is_err());
        for required in ["iss", "aud", "exp", "iat", "sub"] {
            assert!(
                claims(json!({ required: null })).is_err(),
                "{required} required"
            );
        }
    }

    #[test]
    fn scope_check() {
        let scopes = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert!(plan_usage(&scopes(SCOPE)));
        assert!(!plan_usage(&scopes("openid profile email offline_access")));
    }

    #[test]
    fn token_response_parsing() {
        let body = r#"{"access_token":"at","refresh_token":"rt","id_token":"it","token_type":"bearer",
            "expires_in":3600,"scope":"openid chatgpt.tokens.use.direct","earliest_refresh_at":"x"}"#;
        let g = parse_token_response(body, 100, None).unwrap();
        assert_eq!(g.expires_at, 3700);
        assert_eq!(g.scopes, ["openid", "chatgpt.tokens.use.direct"]);
        assert_eq!(g.refresh_token.as_deref(), Some("rt"));

        let refresh = r#"{"access_token":"at2","token_type":"Bearer","expires_in":60}"#;
        let previous = vec!["openid".to_string()];
        let g = parse_token_response(refresh, 0, Some(&previous)).unwrap();
        assert_eq!(g.scopes, previous);
        assert!(g.refresh_token.is_none());

        assert!(
            parse_token_response(refresh, 0, None).is_err(),
            "scope required on exchange"
        );
        let mac = r#"{"access_token":"a","token_type":"mac","expires_in":60,"scope":"openid"}"#;
        assert!(parse_token_response(mac, 0, None).is_err());
        let zero = r#"{"access_token":"a","token_type":"Bearer","expires_in":0,"scope":"openid"}"#;
        assert!(parse_token_response(zero, 0, None).is_err());
        let huge = r#"{"access_token":"a","token_type":"Bearer","expires_in":9223372036854775807,"scope":"openid"}"#;
        assert_eq!(
            parse_token_response(huge, 100, None).unwrap().expires_at,
            i64::MAX
        );
    }

    #[test]
    fn refresh_errors() {
        assert_eq!(oauth_error(r#"{"error":"invalid_grant"}"#), "invalid_grant");
        assert_eq!(
            oauth_error(r#"{"error":{"code":"refresh_token_expired","message":"x"}}"#),
            "refresh_token_expired"
        );
        assert_eq!(
            oauth_error(r#"{"code":"refresh_token_reused"}"#),
            "refresh_token_reused"
        );
        assert_eq!(oauth_error("<html>"), "");
        assert!(refresh_is_terminal(400, r#"{"error":"invalid_grant"}"#));
        assert!(refresh_is_terminal(
            400,
            r#"{"error":{"code":"refresh_token_invalidated"}}"#
        ));
        assert!(refresh_is_terminal(400, r#"{"code":"token_expired"}"#));
        assert!(refresh_is_terminal(401, "<html>"), "any 401 is terminal");
        assert!(!refresh_is_terminal(400, r#"{"error":"invalid_client"}"#));
        assert!(!refresh_is_terminal(429, ""));
        assert!(matches!(endpoint_error(429, ""), AuthError::Network(_)));
        assert_eq!(
            endpoint_error(403, "<html>blocked</html>").to_string(),
            "Sign-in failed: the request was rejected (HTTP 403)."
        );
    }

    #[test]
    fn refresh_response_is_lenient_but_sign_in_is_strict() {
        let body = r#"{"access_token":"a2","refresh_token":"NEW"}"#;
        let previous = ["openid".to_string()];
        let grant = parse_token_response(body, 100, Some(&previous)).unwrap();
        assert_eq!(grant.refresh_token.as_deref(), Some("NEW"));
        assert_eq!(grant.expires_at, 100 + DEFAULT_LIFETIME_SECS);
        assert_eq!(grant.scopes, previous);
        assert!(parse_token_response(body, 100, None).is_err());
    }

    // RFC 7515 appendix A.2.
    const RFC_JWS: &str = concat!(
        "eyJhbGciOiJSUzI1NiJ9",
        ".eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODAsDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ",
        ".cC4hiUPoj9Eetdgtv3hF80EGrhuB__dzERat0XF9g2VtQgr9PJbu3XOiZj5RZmh7AAuHIm4Bh-0Qc_lF5YKt_O8W2Fp5",
        "jujGbds9uJdbF9CUAr7t1dnZcAcQjbKBYNX4BAynRFdiuB--f_nZLgrnbyTyWzO75vRK5h6xBArLIARNPvkSjtQBMHlb",
        "1L07Qe7K0GarZRmB_eSN9383LcOLn6_dO--xi12jzDwusC-eOkHWEsqtFZESc6BfI7noOPqvhJ1phCnvWh6IeYI2w9QO",
        "YEUipUTI8np6LbgGY9Fs98rqVt5AXLIhWkWywlVmtVrBp0igcN_IoypGlUPQGe77Rw",
    );
    const RFC_N: &str = concat!(
        "ofgWCuLjybRlzo0tZWJjNiuSfb4p4fAkd_wWJcyQoTbji9k0l8W26mPddxHmfHQp-Vaw-4qPCJrcS2mJPMEzP1Pt0Bm4",
        "d4QlL-yRT-SFd2lZS-pCgNMsD1W_YpRPEwOWvG6b32690r2jZ47soMZo9wGzjb_7OMg0LOL-bSf63kpaSHSXndS5z5re",
        "xMdbBYUsLA9e-KXBdQOS-UTo7WTBEMa2R2CapHg665xsmtdVMTBQY4uDZlxvb3qCo5ZwKh9kG4LT6_I5IhlJH7aGhyxX",
        "FvUK-DWNmoudF8NAco9_h9iaGNj8q2ethFkMLs91kzk2PAcDTW9gb54h4FRWyuXpoQ",
    );

    #[test]
    fn rs256_signature() {
        let key = Jwk {
            kid: String::new(),
            kty: "RSA".into(),
            n: RFC_N.into(),
            e: "AQAB".into(),
        };
        let payload = verify_rs256(RFC_JWS, &key).unwrap();
        assert!(payload.starts_with(br#"{"iss":"joe""#));

        let (header, rest) = RFC_JWS.split_once('.').unwrap();
        let signature = rest.split_once('.').unwrap().1;
        let tampered = format!("{header}.{}.{signature}", B64.encode(br#"{"iss":"eve"}"#));
        assert!(verify_rs256(&tampered, &key).is_err());
        let hs256 = format!("{}.{rest}", B64.encode(br#"{"alg":"HS256"}"#));
        assert!(verify_rs256(&hs256, &key).is_err());
    }

    #[test]
    fn signed_out_auth_and_registration_file() {
        let dir = std::env::temp_dir().join(format!("patina-auth-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fn shareable<T: Send + Sync>() {}
        shareable::<Auth>();
        let auth = Auth::new(&dir, false);
        assert_eq!(auth.account(), None);
        assert_eq!(auth.access_token(), Err(AuthError::SignInRequired));
        assert!(auth.sign_out());

        let reg = lock(&auth.state).reg.clone();
        assert!(reg.ext_agent_host_id.starts_with("urn:uuid:"));
        auth.save_registration(&reg).unwrap();
        let mode = fs::metadata(dir.join("account.json"))
            .unwrap()
            .permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
        let reloaded = Auth::new(&dir, false);
        assert_eq!(
            lock(&reloaded.state).reg.ext_agent_host_id,
            reg.ext_agent_host_id
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Loopback only: a stray request gets 400, then the real callback is returned.
    #[test]
    fn callback_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let get = move |target: &str| {
            let mut s = TcpStream::connect(addr).unwrap();
            write!(s, "GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut response = String::new();
            s.read_to_string(&mut response).unwrap();
            response
        };
        let client = thread::spawn(move || {
            let stray = get("/favicon.ico");
            let good = get("/auth/callback?code=c&state=s&client_id=oaiapp_1");
            (stray, good)
        });
        let cancel = AtomicBool::new(false);
        let (stream, cb) = wait_for_callback(&listener, "s", None, "Patina", &cancel).unwrap();
        assert_eq!((cb.code.as_str(), cb.client_id.as_str()), ("c", "oaiapp_1"));
        respond(&stream, "200 OK", "done");
        drop(stream);
        let (stray, good) = client.join().unwrap();
        assert!(stray.starts_with("HTTP/1.1 400"));
        assert!(good.starts_with("HTTP/1.1 200") && good.ends_with("done</p>"));

        cancel.store(true, Ordering::Relaxed);
        let cancelled = wait_for_callback(&listener, "s", None, "Patina", &cancel);
        assert!(matches!(cancelled, Err(AuthError::Cancelled)));
    }
}
