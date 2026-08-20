use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use data_encoding::BASE32_NOPAD;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::error::AppError;

const MAX_PAIR_FAILURES: u32 = 5;
const PAIR_LOCKOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct AuthManager {
    inner: Arc<RwLock<AuthState>>,
    auth_ttl: Duration,
    ticket_ttl: Duration,
}

struct AuthState {
    pairing_hash: Option<[u8; 32]>,
    manual_pairing_hash: Option<[u8; 32]>,
    sessions: HashMap<[u8; 32], AuthSession>,
    tickets: HashMap<[u8; 32], Ticket>,
    failed_pairings: u32,
    locked_until: Option<Instant>,
    generation: u64,
}

#[derive(Clone)]
struct AuthSession {
    client_id: Uuid,
    csrf_hash: [u8; 32],
    expires_at: Instant,
    generation: u64,
}

struct Ticket {
    client_id: Uuid,
    expires_at: Instant,
    generation: u64,
    purpose: TicketPurpose,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketPurpose {
    Events,
    #[default]
    Signaling,
}

#[derive(Debug, Clone)]
pub struct AuthContext {
    pub client_id: Uuid,
    generation: u64,
}

#[derive(Debug)]
pub struct PairingMaterial {
    pub secret: Zeroizing<String>,
    pub manual_code: Zeroizing<String>,
}

#[derive(Debug)]
pub struct ExchangeResult {
    pub session_token: Zeroizing<String>,
    pub csrf_token: Zeroizing<String>,
    pub client_id: Uuid,
    pub expires_in_seconds: u64,
}

impl AuthManager {
    pub fn new(auth_ttl: Duration, ticket_ttl: Duration) -> Self {
        Self {
            inner: Arc::new(RwLock::new(AuthState {
                pairing_hash: None,
                manual_pairing_hash: None,
                sessions: HashMap::new(),
                tickets: HashMap::new(),
                failed_pairings: 0,
                locked_until: None,
                generation: 0,
            })),
            auth_ttl,
            ticket_ttl,
        }
    }

    pub fn rotate_pairing(&self) -> Result<PairingMaterial> {
        let raw = random_bytes::<32>()?;
        let secret = Zeroizing::new(URL_SAFE_NO_PAD.encode(raw));
        let manual_code = Zeroizing::new(group_manual_code(&raw));
        let pairing_hash = hash(secret.as_bytes());
        let manual_pairing_hash = hash(manual_code.as_bytes());

        let mut state = self.inner.write().expect("auth state lock poisoned");
        state.generation = state.generation.wrapping_add(1);
        state.pairing_hash = Some(pairing_hash);
        state.manual_pairing_hash = Some(manual_pairing_hash);
        state.sessions.clear();
        state.tickets.clear();
        state.failed_pairings = 0;
        state.locked_until = None;

        Ok(PairingMaterial {
            secret,
            manual_code,
        })
    }

    pub fn write_pairing_file(
        &self,
        material: &PairingMaterial,
        path: impl AsRef<Path>,
        public_base_url: &str,
    ) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let temporary = path.with_extension("tmp");
        let body = format!(
            "PAIRING_URL={}#pair={}\nMANUAL_CODE={}\n",
            public_base_url.trim_end_matches('/'),
            material.secret.as_str(),
            material.manual_code.as_str()
        );

        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to publish {}", path.display()))?;
        Ok(())
    }

    pub fn exchange_pairing(&self, presented: &str) -> Result<ExchangeResult, AppError> {
        let now = Instant::now();
        let presented_hash = hash(presented.as_bytes());
        let mut state = self.inner.write().expect("auth state lock poisoned");

        if state.locked_until.is_some_and(|until| until > now) {
            return Err(AppError::InvalidPairing);
        }
        state.locked_until = None;

        let secret_valid = state
            .pairing_hash
            .map(|expected| bool::from(expected.ct_eq(&presented_hash)))
            .unwrap_or(false);
        let manual_valid = state
            .manual_pairing_hash
            .map(|expected| bool::from(expected.ct_eq(&presented_hash)))
            .unwrap_or(false);
        let valid = secret_valid | manual_valid;
        if !valid {
            state.failed_pairings = state.failed_pairings.saturating_add(1);
            if state.failed_pairings >= MAX_PAIR_FAILURES {
                state.failed_pairings = 0;
                state.locked_until = Some(now + PAIR_LOCKOUT);
            }
            return Err(AppError::InvalidPairing);
        }

        state.pairing_hash = None;
        state.manual_pairing_hash = None;
        state.failed_pairings = 0;
        let session_token = Zeroizing::new(random_token().map_err(|_| AppError::Internal)?);
        let csrf_token = Zeroizing::new(random_token().map_err(|_| AppError::Internal)?);
        let client_id = Uuid::new_v4();
        let generation = state.generation;
        state.sessions.insert(
            hash(session_token.as_bytes()),
            AuthSession {
                client_id,
                csrf_hash: hash(csrf_token.as_bytes()),
                expires_at: now + self.auth_ttl,
                generation,
            },
        );

        Ok(ExchangeResult {
            session_token,
            csrf_token,
            client_id,
            expires_in_seconds: self.auth_ttl.as_secs(),
        })
    }

    pub fn authenticate(
        &self,
        session_token: Option<&str>,
        csrf_token: Option<&str>,
        require_csrf: bool,
    ) -> Result<AuthContext, AppError> {
        let token = session_token.ok_or(AppError::Unauthorized)?;
        let session_hash = hash(token.as_bytes());
        let now = Instant::now();
        let mut state = self.inner.write().expect("auth state lock poisoned");
        prune_expired(&mut state, now);
        let session = state
            .sessions
            .get(&session_hash)
            .ok_or(AppError::Unauthorized)?;

        if require_csrf {
            let presented = csrf_token.ok_or(AppError::InvalidCsrf)?;
            if !bool::from(session.csrf_hash.ct_eq(&hash(presented.as_bytes()))) {
                return Err(AppError::InvalidCsrf);
            }
        }

        Ok(AuthContext {
            client_id: session.client_id,
            generation: session.generation,
        })
    }

    pub fn validate_context(&self, context: &AuthContext) -> Result<(), AppError> {
        let now = Instant::now();
        let mut state = self.inner.write().expect("auth state lock poisoned");
        prune_expired(&mut state, now);
        if context.generation == state.generation
            && state.sessions.values().any(|session| {
                session.client_id == context.client_id
                    && session.generation == context.generation
                    && session.expires_at > now
            })
        {
            Ok(())
        } else {
            Err(AppError::Unauthorized)
        }
    }

    pub fn issue_ticket(
        &self,
        context: &AuthContext,
        purpose: TicketPurpose,
    ) -> Result<Zeroizing<String>, AppError> {
        let token = Zeroizing::new(random_token().map_err(|_| AppError::Internal)?);
        let mut state = self.inner.write().expect("auth state lock poisoned");
        let now = Instant::now();
        prune_expired(&mut state, now);
        let generation = state.generation;
        if context.generation != generation
            || !state.sessions.values().any(|session| {
                session.client_id == context.client_id
                    && session.generation == generation
                    && session.expires_at > now
            })
        {
            return Err(AppError::Unauthorized);
        }
        state.tickets.insert(
            hash(token.as_bytes()),
            Ticket {
                client_id: context.client_id,
                expires_at: now + self.ticket_ttl,
                generation,
                purpose,
            },
        );
        Ok(token)
    }

    pub fn consume_ticket(
        &self,
        token: &str,
        expected_purpose: TicketPurpose,
    ) -> Result<AuthContext, AppError> {
        let mut state = self.inner.write().expect("auth state lock poisoned");
        let ticket = state
            .tickets
            .remove(&hash(token.as_bytes()))
            .ok_or(AppError::Unauthorized)?;
        if ticket.expires_at <= Instant::now()
            || ticket.generation != state.generation
            || ticket.purpose != expected_purpose
        {
            return Err(AppError::Unauthorized);
        }
        Ok(AuthContext {
            client_id: ticket.client_id,
            generation: ticket.generation,
        })
    }

    pub fn revoke_all(&self) {
        let mut state = self.inner.write().expect("auth state lock poisoned");
        state.sessions.clear();
        state.tickets.clear();
    }

    /// Marks a newly generated pairing as unavailable when its root-only
    /// handoff file could not be published. The watchdog can then retry with
    /// an entirely new generation instead of leaving an unreachable secret.
    pub(crate) fn invalidate_unpublished_pairing(&self) {
        let mut state = self.inner.write().expect("auth state lock poisoned");
        state.pairing_hash = None;
        state.manual_pairing_hash = None;
        state.sessions.clear();
        state.tickets.clear();
    }

    /// Returns true when one-time pairing has been consumed and every
    /// controller session has expired. The lifecycle watchdog uses this to
    /// burn any abandoned browser state and publish fresh pairing material.
    pub fn pairing_recovery_required(&self) -> bool {
        let now = Instant::now();
        let mut state = self.inner.write().expect("auth state lock poisoned");
        prune_expired(&mut state, now);
        let generation = state.generation;

        generation > 0
            && state.pairing_hash.is_none()
            && state.manual_pairing_hash.is_none()
            && state.sessions.is_empty()
    }

    pub fn pairing_available(&self) -> bool {
        let state = self.inner.read().expect("auth state lock poisoned");
        state.pairing_hash.is_some() || state.manual_pairing_hash.is_some()
    }
}

fn prune_expired(state: &mut AuthState, now: Instant) {
    let generation = state.generation;
    state
        .sessions
        .retain(|_, session| session.expires_at > now && session.generation == generation);
    state
        .tickets
        .retain(|_, ticket| ticket.expires_at > now && ticket.generation == generation);
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| anyhow::anyhow!("OS CSPRNG failed: {error}"))?;
    Ok(bytes)
}

fn random_token() -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes::<32>()?))
}

fn hash(value: &[u8]) -> [u8; 32] {
    *blake3::hash(value).as_bytes()
}

fn group_manual_code(bytes: &[u8; 32]) -> String {
    let encoded = BASE32_NOPAD.encode(bytes);
    encoded
        .as_bytes()
        .chunks(4)
        .map(|chunk| std::str::from_utf8(chunk).expect("base32 is ASCII"))
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> AuthManager {
        AuthManager::new(Duration::from_secs(600), Duration::from_secs(30))
    }

    #[test]
    fn pairing_is_single_use_and_csrf_is_checked() {
        let auth = manager();
        let pairing = auth.rotate_pairing().unwrap();
        let exchange = auth.exchange_pairing(pairing.secret.as_str()).unwrap();
        assert!(auth.exchange_pairing(pairing.secret.as_str()).is_err());
        assert!(
            auth.authenticate(
                Some(exchange.session_token.as_str()),
                Some(exchange.csrf_token.as_str()),
                true,
            )
            .is_ok()
        );
        assert!(
            auth.authenticate(Some(exchange.session_token.as_str()), Some("wrong"), true)
                .is_err()
        );
    }

    #[test]
    fn rotation_revokes_sessions_and_tickets() {
        let auth = manager();
        let pairing = auth.rotate_pairing().unwrap();
        let exchange = auth.exchange_pairing(pairing.secret.as_str()).unwrap();
        let context = auth
            .authenticate(Some(exchange.session_token.as_str()), None, false)
            .unwrap();
        let ticket = auth
            .issue_ticket(&context, TicketPurpose::Signaling)
            .unwrap();
        auth.rotate_pairing().unwrap();
        assert!(
            auth.issue_ticket(&context, TicketPurpose::Signaling)
                .is_err()
        );
        assert!(
            auth.consume_ticket(ticket.as_str(), TicketPurpose::Signaling)
                .is_err()
        );
        assert!(
            auth.authenticate(Some(exchange.session_token.as_str()), None, false)
                .is_err()
        );
    }

    #[test]
    fn ticket_purpose_is_bound_and_single_use() {
        let auth = manager();
        let pairing = auth.rotate_pairing().unwrap();
        let exchange = auth.exchange_pairing(pairing.secret.as_str()).unwrap();
        let context = auth
            .authenticate(Some(exchange.session_token.as_str()), None, false)
            .unwrap();
        let ticket = auth.issue_ticket(&context, TicketPurpose::Events).unwrap();
        assert!(
            auth.consume_ticket(ticket.as_str(), TicketPurpose::Signaling)
                .is_err()
        );
        assert!(
            auth.consume_ticket(ticket.as_str(), TicketPurpose::Events)
                .is_err()
        );
    }

    #[test]
    fn manual_code_has_full_256_bit_material() {
        let auth = manager();
        let pairing = auth.rotate_pairing().unwrap();
        assert_eq!(pairing.manual_code.replace('-', "").len(), 52);
    }

    #[test]
    fn manual_code_can_be_exchanged_once() {
        let auth = manager();
        let pairing = auth.rotate_pairing().unwrap();
        assert!(auth.exchange_pairing(pairing.manual_code.as_str()).is_ok());
        assert!(auth.exchange_pairing(pairing.manual_code.as_str()).is_err());
    }

    #[test]
    fn consumed_pairing_requires_recovery_after_auth_expiry() {
        let auth = AuthManager::new(Duration::from_secs(1), Duration::from_secs(1));
        let pairing = auth.rotate_pairing().unwrap();
        auth.exchange_pairing(pairing.secret.as_str()).unwrap();
        assert!(!auth.pairing_recovery_required());

        std::thread::sleep(Duration::from_millis(1_100));
        assert!(auth.pairing_recovery_required());

        auth.rotate_pairing().unwrap();
        assert!(!auth.pairing_recovery_required());
    }

    #[test]
    fn unpublished_pairing_is_recoverable() {
        let auth = manager();
        auth.rotate_pairing().unwrap();
        auth.invalidate_unpublished_pairing();
        assert!(!auth.pairing_available());
        assert!(auth.pairing_recovery_required());
    }
}
