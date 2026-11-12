//! The permission gate: tiers, the host's policy, and confirmation through
//! the client.
//!
//! Every `tools/call` passes the gate after its arguments validate and before
//! its handler runs:
//!
//! 1. The host's [`Policy`] decides: [`Decision::Allow`], [`Decision::Deny`],
//!    or [`Decision::Confirm`].
//! 2. A `Publish` tool is never allowed outright: `Allow` is treated as
//!    `Confirm`. No policy, setting, or earlier approval can skip it.
//! 3. `Confirm` asks the user through the client, with MCP's own mechanism.
//!    The question shows the call's arguments in full, as the tool
//!    receives them (members in the order sent, numbers as spelled,
//!    non-ASCII and control characters escaped), never a part of them; a
//!    call too large to show is refused. Then:
//!    on 2026-07-28, the call is answered with an `InputRequiredResult`
//!    carrying a form-mode `elicitation/create` request and a single-use
//!    `requestState`, and the tool runs only when the client retries the call
//!    with the user's acceptance and that state. On a legacy (2025-11-25)
//!    session, the server sends `elicitation/create` itself and waits for the
//!    answer. A client that did not declare form elicitation gets
//!    `MissingRequiredClientCapability` (`-32021`) on 2026-07-28, and a tool
//!    error on a legacy session; the tool never runs unconfirmed.
//!
//! # What this does and does not guarantee
//!
//! The gate runs inside the server, so a model cannot call a tool the policy
//! denies, cannot run a confirm-tier tool without a confirmation the server
//! itself issued for exactly that tool and those arguments, and cannot reuse
//! one confirmation for a second call. The confirmation itself is answered
//! by the client: a conforming client shows the question to its user, but
//! the server cannot verify that a human (rather than an automated client)
//! answered. Tool annotations are hints the client must not trust. And the
//! gate is not a sandbox: a handler that runs has the server process's
//! privileges.
//!
//! # The confirmation state
//!
//! `requestState` is a random 128-bit token naming an entry the server keeps:
//! the tool's registration, a keyed 128-bit digest of the call's arguments
//! as shown, and an expiry. Nothing in the token itself is trusted. A
//! tampered or replayed token names no entry and is refused (tokens are
//! compared in constant time); an entry is
//! removed when it is presented, so each confirmation runs at most one call.
//! Entries expire (10 minutes by default), are capped per connection (64 by
//! default, the oldest forgotten past it) and in all (4096 by default, new
//! confirmations refused past it), and are forgotten when the connection
//! they were issued on closes. Every operation on them costs the same
//! however many there are.
//!
//! The retry is decided again: the policy sees it (with
//! [`PermissionRequest::is_retry`]), and a [`Decision::Deny`] stops it, so
//! a policy changed after the question was asked still applies. `Allow` and
//! `Confirm` both let the retry go on to redeem its confirmation.
//!
//! On stdio there is no authenticated principal to bind; the token is the
//! capability, and it is sent only to the client that made the call.
//!
//! Tokens are the keyed SipHash of a counter that never repeats, and the
//! digest is a keyed SipHash of the arguments, so a client can neither
//! predict a token nor search for other arguments with the same digest. The
//! keys are fixed once per process, each taken from the standard library's
//! hash seed on a thread of its own, which the standard library draws from
//! the operating system's random source. This is not a dedicated CSPRNG
//! API: the standard library does not promise how it derives those keys.
//!
//! # Prompt injection
//!
//! The arguments in a confirmation are written by the model, not by the
//! server, and a model can be talked into writing anything. The question
//! shows the tool's own wording, then the tool's name and tier,
//! then the arguments as data (escaped, in full, after `Arguments:`), but a
//! user who skims can still read an argument such as
//! `"prod (dry run, nothing is deleted)"` as the server's own words. Keep
//! the [`Tool::confirmation`](crate::Tool::confirmation) question about
//! what the tool does, so it never depends on the arguments to be safe.
//!
//! In the other direction, a tool's results go to the model as they are.
//! A tool that relays text from outside (web pages, files, issues, mail)
//! hands the model untrusted input that may carry instructions. This crate
//! does not mark or filter it: that is the host's job. Mark relayed content
//! for what it is (for example with `annotations.audience`, or a wrapper
//! that says where the text came from), and keep the policy and the tiers,
//! not the model's judgment, between the model and anything destructive.

use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque, hash_map::RandomState},
    hash::{BuildHasher, Hasher},
    sync::{
        Mutex, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{Tier, jsonx::Value, protocol::Era};

/// What the host's [`Policy`] says about one call.
///
/// # Examples
///
/// ```
/// use mcp_io::Decision;
///
/// let deny = Decision::deny("writes are disabled in this workspace");
/// assert!(matches!(deny, Decision::Deny(_)));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Decision {
    /// Run it (a `Publish` tool still asks the user).
    Allow,
    /// Refuse it, with a reason the model is shown.
    Deny(Cow<'static, str>),
    /// Ask the user through the client first.
    Confirm,
}

impl Decision {
    /// [`Decision::Deny`] with `reason`.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(mcp_io::Decision::deny("no"), mcp_io::Decision::Deny("no".into()));
    /// ```
    #[must_use]
    pub fn deny(reason: impl Into<Cow<'static, str>>) -> Self {
        Self::Deny(reason.into())
    }
}

/// One call, as the [`Policy`] sees it: after its arguments validated
/// against the tool's input schema.
#[derive(Debug)]
pub struct PermissionRequest<'a> {
    pub(crate) tool: &'a str,
    pub(crate) tier: Tier,
    pub(crate) arguments: &'a Value<'a>,
    pub(crate) client_info: Option<&'a Value<'a>>,
    pub(crate) era: Era,
    pub(crate) retry: bool,
}

impl<'a> PermissionRequest<'a> {
    /// The tool's name.
    #[must_use]
    pub fn tool(&self) -> &'a str {
        self.tool
    }

    /// The tool's tier.
    #[must_use]
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// The call's arguments (valid against the input schema).
    #[must_use]
    pub fn arguments(&self) -> &'a Value<'a> {
        self.arguments
    }

    /// The client's self-reported `clientInfo` (`name`, `version`), if it
    /// sent one. Self-reported and unverified: for display and logs, never
    /// for a security decision.
    #[must_use]
    pub fn client_info(&self) -> Option<&'a Value<'a>> {
        self.client_info
    }

    /// The protocol era the call is served under.
    #[must_use]
    pub fn era(&self) -> Era {
        self.era
    }

    /// Whether this is a 2026-07-28 retry: the call again, carrying the
    /// `requestState` of a confirmation the server asked for. A policy that
    /// counts calls can skip retries, so a confirmed call counts once.
    /// [`Decision::Deny`] stops a retry; `Allow` and `Confirm` let it go on
    /// to its confirmation.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Decision, PermissionRequest};
    ///
    /// let policy = |request: &PermissionRequest<'_>| {
    ///     if request.is_retry() {
    ///         // Already counted when the confirmation was asked for.
    ///         return Decision::Allow;
    ///     }
    ///     Decision::Confirm
    /// };
    /// # let _ = policy;
    /// ```
    #[must_use]
    pub fn is_retry(&self) -> bool {
        self.retry
    }
}

/// The host's say over every tool call, consulted after validation and
/// before the handler (see the [module documentation](crate::permission)).
///
/// Implement it for authorization, per-workspace switches, or rate limits
/// (MCP: servers "MUST rate limit tool invocations"; [`Decision::Deny`]
/// with a retry hint is how). A closure `Fn(&PermissionRequest) -> Decision`
/// is a policy. A policy that panics denies the call.
///
/// It is consulted for every call. On 2026-07-28 a confirmed call arrives
/// twice (the call, then its retry with the `requestState` and the user's
/// answer), and the policy sees both: the retry with
/// [`PermissionRequest::is_retry`] set. A [`Decision::Deny`] stops the
/// retry, so a policy changed after the question was asked still applies;
/// `Allow` and `Confirm` let the retry go on, and it runs only if it redeems
/// the confirmation the first call issued.
///
/// # Examples
///
/// ```
/// use std::sync::Mutex;
/// use mcp_io::{Decision, PermissionRequest, Policy, Tier};
///
/// /// Read-only tools run; at most 100 other calls are allowed, each confirmed
/// /// (one asked for and declined counts too, and a retry is not counted
/// /// again).
/// struct Budgeted { left: Mutex<u32> }
///
/// impl Policy for Budgeted {
///     fn decide(&self, request: &PermissionRequest<'_>) -> Decision {
///         if request.tier() == Tier::ReadOnly || request.is_retry() {
///             return Decision::Allow;
///         }
///         let mut left = self.left.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
///         match left.checked_sub(1) {
///             Some(rest) => {
///                 *left = rest;
///                 Decision::Confirm
///             }
///             None => Decision::deny("the session's budget of changes is spent"),
///         }
///     }
/// }
/// # let _ = Budgeted { left: Mutex::new(100) };
/// ```
pub trait Policy: Send + Sync + 'static {
    /// Decides one call.
    fn decide(&self, request: &PermissionRequest<'_>) -> Decision;
}

impl<F> Policy for F
where
    F: Fn(&PermissionRequest<'_>) -> Decision + Send + Sync + 'static,
{
    fn decide(&self, request: &PermissionRequest<'_>) -> Decision {
        self(request)
    }
}

/// The default policy: tools up to the ceiling tier run, tools above it ask
/// the user. `Ceiling(Tier::Mutating)` is the default (`Destructive` and
/// `Publish` ask).
///
/// # Examples
///
/// ```
/// use mcp_io::{Ceiling, Implementation, Server, Tier};
///
/// // Ask before anything that is not read-only:
/// let server = Server::new(Implementation::new("demo", "1.0.0")).policy(Ceiling(Tier::ReadOnly));
/// # let _ = server;
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ceiling(pub Tier);

impl Default for Ceiling {
    fn default() -> Self {
        Self(Tier::Mutating)
    }
}

impl Policy for Ceiling {
    fn decide(&self, request: &PermissionRequest<'_>) -> Decision {
        if request.tier <= self.0 {
            Decision::Allow
        } else {
            Decision::Confirm
        }
    }
}

/// The gate's verdict once the tier rules are applied to the policy's
/// decision.
pub(crate) fn effective(tier: Tier, decision: Decision) -> Decision {
    match decision {
        Decision::Allow if tier == Tier::Publish => Decision::Confirm,
        other => other,
    }
}

/// A confirmation the server issued and has not seen used.
struct Pending {
    token: String,
    /// Its place in issue order, which is also expiry order: every entry of
    /// a ledger has the same lifetime.
    seq: u64,
    /// The connection it was issued on, for its quota. It may be redeemed
    /// on any open connection (MCP does not tie requests to connections),
    /// and it is forgotten when the connection it was issued on closes.
    connection: u64,
    binding: Binding,
    /// `None`: a timeout too large to add to the clock; it never expires.
    expires: Option<Instant>,
}

/// What a confirmation is bound to: the tool's registration and a keyed
/// digest of the call's arguments as shown. The digest, not the text, is
/// kept, so an entry costs the same whatever the arguments' size.
#[derive(PartialEq, Eq)]
struct Binding {
    tool: Box<str>,
    /// Which registration of the tool: a tool removed and registered again
    /// under the same name does not inherit confirmations.
    generation: u64,
    digest: [u64; 2],
    length: usize,
}

/// Why a presented `requestState` was refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Redeem {
    /// Unknown, expired, or already used.
    Unknown,
    /// Issued for another tool, registration, or other arguments.
    Mismatch,
}

/// Why a confirmation was not issued.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The server holds as many outstanding confirmations as it may.
    Full,
    /// The connection the call came in on has closed.
    Closed,
}

/// A place in an issue order: an entry's `seq` and its key. A place whose
/// entry has since gone (redeemed, evicted, or expired) is stale; stale
/// places are skipped when met and swept out once they outnumber the live
/// ones, so every operation costs O(1) amortized.
type Place = (u64, u128);

/// Stale places tolerated beyond the live count before a sweep.
const SLACK: usize = 32;

/// One open connection's confirmations.
#[derive(Default)]
struct Issued {
    /// Its places, in issue order.
    order: VecDeque<Place>,
    /// How many of them are live.
    live: usize,
}

/// The ledger's state, behind its lock.
#[derive(Default)]
struct Inner {
    /// Live entries, by a keyed digest of their token.
    entries: HashMap<u128, Pending>,
    /// Every entry's place, in issue (and so expiry) order.
    order: VecDeque<Place>,
    /// Open connections. A confirmation is issued only on one of these.
    connections: HashMap<u64, Issued>,
    next: u64,
}

impl Inner {
    fn is_live(entries: &HashMap<u128, Pending>, (seq, key): Place) -> bool {
        entries.get(&key).is_some_and(|p| p.seq == seq)
    }

    /// Removes the entry `key` names, keeping its connection's count.
    fn take(&mut self, key: u128) -> Option<Pending> {
        let entry = self.entries.remove(&key)?;
        if let Some(issued) = self.connections.get_mut(&entry.connection) {
            issued.live = issued.live.saturating_sub(1);
        }
        Some(entry)
    }

    /// Drops the expired entries, which are the oldest, and sweeps stale
    /// places out of the issue order once there are enough of them.
    fn expire(&mut self, now: Instant) {
        while let Some(&(seq, key)) = self.order.front() {
            let expired = match self.entries.get(&key) {
                Some(p) if p.seq == seq => p.expires.is_some_and(|at| at <= now),
                // Stale: skip it.
                _ => true,
            };
            if !expired {
                // The oldest live entry has not expired, so none has.
                break;
            }
            if Self::is_live(&self.entries, (seq, key)) {
                let _expired = self.take(key);
            }
            let _done = self.order.pop_front();
        }
        if self.order.len() > self.entries.len().saturating_mul(2) + SLACK {
            let entries = &self.entries;
            self.order.retain(|&place| Self::is_live(entries, place));
        }
    }

    /// Removes `connection`'s oldest live entry.
    fn evict_oldest(&mut self, connection: u64) {
        loop {
            let Some(place) = self
                .connections
                .get_mut(&connection)
                .and_then(|issued| issued.order.pop_front())
            else {
                return;
            };
            if Self::is_live(&self.entries, place) {
                let _evicted = self.take(place.1);
                return;
            }
        }
    }
}

/// The server's outstanding confirmations, shared by every connection.
///
/// Entries are found by a keyed digest of their token, so issuing,
/// redeeming, expiring, and evicting cost O(1) amortized whatever the
/// number of entries; a connection's entries go when it closes.
pub(crate) struct Ledger {
    inner: Mutex<Inner>,
    /// Outstanding confirmations per connection.
    capacity: usize,
    /// Outstanding confirmations in all.
    total: usize,
    ttl: Duration,
}

impl Ledger {
    pub(crate) fn new(capacity: usize, total: usize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            capacity: capacity.max(1),
            total: total.max(1),
            ttl,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn bind(tool: &str, generation: u64, arguments: &str) -> Binding {
        Binding {
            tool: tool.into(),
            generation,
            digest: digest(&keys().digest, b'a', arguments.as_bytes()),
            length: arguments.len(),
        }
    }

    /// Where a token's entry is kept: a keyed digest of the token, so the
    /// lookup's timing says nothing about the tokens held.
    fn key(token: &str) -> u128 {
        let [high, low] = digest(&keys().digest, b'k', token.as_bytes());
        (u128::from(high) << 64) | u128::from(low)
    }

    /// Starts keeping confirmations for `connection`.
    pub(crate) fn open(&self, connection: u64) {
        let _opened = self.lock().connections.entry(connection).or_default();
    }

    /// Forgets `connection` and every confirmation issued on it.
    pub(crate) fn close(&self, connection: u64) {
        let mut inner = self.lock();
        if let Some(issued) = inner.connections.remove(&connection) {
            for place in issued.order {
                if Inner::is_live(&inner.entries, place) {
                    let _forgotten = inner.entries.remove(&place.1);
                }
            }
        }
        inner.expire(Instant::now());
    }

    /// Records a confirmation for registration `generation` of `tool` with
    /// `arguments` (as shown), issued on `connection`, and returns its
    /// token. Expired entries go first; past the connection's quota, its
    /// oldest entry goes, so one connection cannot evict another's; past
    /// the server's total, nothing is issued.
    pub(crate) fn issue(
        &self,
        connection: u64,
        tool: &str,
        generation: u64,
        arguments: &str,
    ) -> Result<String, Refused> {
        let mut token = token();
        let binding = Self::bind(tool, generation, arguments);
        let mut inner = self.lock();
        // The clock is read under the lock, so issue order is expiry order.
        let now = Instant::now();
        inner.expire(now);
        let Some(issued) = inner.connections.get(&connection) else {
            return Err(Refused::Closed);
        };
        if issued.live >= self.capacity {
            inner.evict_oldest(connection);
        }
        if inner.entries.len() >= self.total {
            return Err(Refused::Full);
        }
        let mut key = Self::key(&token);
        while inner.entries.contains_key(&key) {
            // Two live tokens with one key: never in practice, but a new
            // token costs nothing.
            token = self::token();
            key = Self::key(&token);
        }
        let seq = inner.next;
        inner.next = seq.wrapping_add(1);
        let _new = inner.entries.insert(
            key,
            Pending {
                token: token.clone(),
                seq,
                connection,
                binding,
                expires: now.checked_add(self.ttl),
            },
        );
        inner.order.push_back((seq, key));
        let Inner {
            entries,
            connections,
            ..
        } = &mut *inner;
        if let Some(issued) = connections.get_mut(&connection) {
            issued.order.push_back((seq, key));
            issued.live += 1;
            if issued.order.len() > issued.live.saturating_mul(2) + SLACK {
                issued.order.retain(|&place| Inner::is_live(entries, place));
            }
        }
        Ok(token)
    }

    /// Consumes the confirmation `token` names, if it was issued for
    /// registration `generation` of `tool` with `arguments` and has not
    /// expired. A token is removed when presented, matching or not, so a
    /// guessed or replayed one cannot be tried twice.
    pub(crate) fn redeem(
        &self,
        token: &str,
        tool: &str,
        generation: u64,
        arguments: &str,
    ) -> Result<(), Redeem> {
        let binding = Self::bind(tool, generation, arguments);
        let key = Self::key(token);
        let now = Instant::now();
        let mut inner = self.lock();
        inner.expire(now);
        let found = inner
            .entries
            .get(&key)
            .is_some_and(|entry| same(&entry.token, token));
        if !found {
            return Err(Redeem::Unknown);
        }
        let Some(entry) = inner.take(key) else {
            return Err(Redeem::Unknown);
        };
        if entry.expires.is_some_and(|at| at <= now) {
            return Err(Redeem::Unknown);
        }
        if entry.binding != binding {
            return Err(Redeem::Mismatch);
        }
        Ok(())
    }

    /// Live entries, and the places the issue orders hold (live and stale).
    #[cfg(test)]
    pub(crate) fn sizes(&self) -> (usize, usize) {
        let inner = self.lock();
        let places = inner.order.len()
            + inner
                .connections
                .values()
                .map(|issued| issued.order.len())
                .sum::<usize>();
        (inner.entries.len(), places)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.sizes().0
    }
}

/// Whether two tokens are equal, in time that depends only on their
/// lengths (which are public: every token is the same length), never on
/// where they first differ.
fn same(issued: &str, presented: &str) -> bool {
    let (a, b) = (issued.as_bytes(), presented.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let difference = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    core::hint::black_box(difference) == 0
}

/// The keys behind tokens and digests, made once per process.
struct Keys {
    token: RandomState,
    digest: RandomState,
}

fn keys() -> &'static Keys {
    static KEYS: OnceLock<Keys> = OnceLock::new();
    KEYS.get_or_init(|| Keys {
        token: fresh_key(),
        digest: fresh_key(),
    })
}

/// SipHash keys unrelated to any other `RandomState`'s. The standard
/// library seeds each thread's hash keys from the operating system's random
/// source when that thread first makes a `RandomState`, and later states on
/// the same thread are derived from that seed; a state made on a thread of
/// its own therefore has keys of its own. If no thread can be started, the
/// current thread's next state is used.
fn fresh_key() -> RandomState {
    std::thread::Builder::new()
        .name("mcp-io-seed".to_owned())
        .spawn(RandomState::new)
        .ok()
        .and_then(|seeding| seeding.join().ok())
        .unwrap_or_default()
}

/// A 128-bit keyed digest of `bytes` under `key`, separated by `domain`.
fn digest(key: &RandomState, domain: u8, bytes: &[u8]) -> [u64; 2] {
    [0u8, 1].map(|half| {
        let mut hasher = key.build_hasher();
        hasher.write_u8(domain);
        hasher.write_u8(half);
        hasher.write(bytes);
        hasher.finish()
    })
}

/// A fresh, unpredictable confirmation token: 128 bits, the keyed hash of a
/// counter that never repeats under keys fixed for the process.
fn token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let [high, low] = digest(&keys().token, b't', &count.to_le_bytes());
    format!("mcp-io.confirm.{high:016x}{low:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonx::json;

    fn request(tier: Tier) -> Decision {
        let arguments = json!({});
        let r = PermissionRequest {
            tool: "t",
            tier,
            arguments: &arguments,
            client_info: None,
            era: Era::Modern,
            retry: false,
        };
        assert_eq!((r.tool(), r.tier(), r.era()), ("t", tier, Era::Modern));
        assert!(r.client_info().is_none() && r.arguments().as_object().is_some());
        Ceiling::default().decide(&r)
    }

    #[test]
    fn test_default_ceiling_allows_up_to_mutating() {
        assert_eq!(request(Tier::ReadOnly), Decision::Allow);
        assert_eq!(request(Tier::Mutating), Decision::Allow);
        assert_eq!(request(Tier::Destructive), Decision::Confirm);
        assert_eq!(request(Tier::Publish), Decision::Confirm);
    }

    #[test]
    fn test_publish_is_never_allowed_outright() {
        assert_eq!(effective(Tier::Publish, Decision::Allow), Decision::Confirm);
        assert_eq!(
            effective(Tier::Publish, Decision::deny("x")),
            Decision::deny("x")
        );
        assert_eq!(
            effective(Tier::Destructive, Decision::Allow),
            Decision::Allow
        );
    }

    #[test]
    fn test_closures_are_policies() {
        let policy = |r: &PermissionRequest<'_>| {
            if r.tool() == "rm" {
                Decision::deny("never")
            } else {
                Decision::Allow
            }
        };
        let arguments = json!({});
        let r = PermissionRequest {
            tool: "rm",
            tier: Tier::Destructive,
            arguments: &arguments,
            client_info: None,
            era: Era::Legacy,
            retry: false,
        };
        assert_eq!(policy.decide(&r), Decision::deny("never"));
    }

    /// A ledger with `connections` open, numbered from 1.
    fn ledger(capacity: usize, total: usize, ttl: Duration, connections: u64) -> Ledger {
        let ledger = Ledger::new(capacity, total, ttl);
        for connection in 1..=connections {
            ledger.open(connection);
        }
        ledger
    }

    fn issue(ledger: &Ledger, connection: u64, arguments: &str) -> String {
        ledger
            .issue(connection, "t", 0, arguments)
            .unwrap_or_default()
    }

    #[test]
    fn test_ledger_tokens_are_single_use_and_bound_to_the_call() {
        let ledger = ledger(8, 64, Duration::from_secs(60), 2);
        let token = ledger.issue(1, "ship", 0, "{}").unwrap_or_default();
        assert!(token.starts_with("mcp-io.confirm.") && token.len() == 15 + 32);
        assert_eq!(ledger.redeem(&token, "ship", 0, "{}"), Ok(()));
        assert_eq!(
            ledger.redeem(&token, "ship", 0, "{}"),
            Err(Redeem::Unknown),
            "replay"
        );
        let token = ledger.issue(1, "ship", 0, "{\"v\":1}").unwrap_or_default();
        assert_eq!(
            ledger.redeem(&token, "ship", 0, "{\"v\":2}"),
            Err(Redeem::Mismatch)
        );
        assert_eq!(
            ledger.redeem(&token, "ship", 0, "{\"v\":1}"),
            Err(Redeem::Unknown),
            "a mismatch burns the token"
        );
        let token = ledger.issue(1, "ship", 0, "{}").unwrap_or_default();
        assert_eq!(
            ledger.redeem(&token, "other", 0, "{}"),
            Err(Redeem::Mismatch)
        );
        let token = ledger.issue(1, "ship", 0, "{}").unwrap_or_default();
        assert_eq!(
            ledger.redeem(&token, "ship", 1, "{}"),
            Err(Redeem::Mismatch),
            "a tool registered again under the same name"
        );
        // Redeemable from another open connection: MCP ties nothing to one.
        let token = ledger.issue(2, "ship", 0, "{}").unwrap_or_default();
        assert_eq!(ledger.redeem(&token, "ship", 0, "{}"), Ok(()));
        assert_eq!(
            ledger.redeem("mcp-io.confirm.forged", "ship", 0, "{}"),
            Err(Redeem::Unknown)
        );
        // A token one character off names no entry, and burns nothing.
        let token = issue(&ledger, 1, "{}");
        let mut altered = token.clone().into_bytes();
        if let Some(byte) = altered.last_mut() {
            *byte = if *byte == b'0' { b'1' } else { b'0' };
        }
        let altered = String::from_utf8(altered).unwrap_or_default();
        assert_eq!(ledger.redeem(&altered, "t", 0, "{}"), Err(Redeem::Unknown));
        assert_eq!(ledger.redeem(&token, "t", 0, "{}"), Ok(()));
        assert_eq!(ledger.len(), 0);
    }

    #[test]
    fn test_ledger_keeps_a_digest_not_the_arguments() {
        let ledger = ledger(8, 64, Duration::from_secs(60), 1);
        let big = "x".repeat(1 << 20);
        let token = ledger.issue(1, "ship", 0, &big).unwrap_or_default();
        let size = core::mem::size_of::<Pending>();
        assert!(
            size < 256,
            "an entry is {size} bytes whatever the arguments"
        );
        let mut other = big.clone();
        other.replace_range(..1, "y");
        assert_eq!(
            ledger.redeem(&token, "ship", 0, &other),
            Err(Redeem::Mismatch)
        );
    }

    #[test]
    fn test_ledger_expiry_and_per_connection_quota() {
        let expired = ledger(8, 64, Duration::ZERO, 1);
        let token = issue(&expired, 1, "{}");
        assert_eq!(expired.redeem(&token, "t", 0, "{}"), Err(Redeem::Unknown));
        for _ in 0..1000 {
            let _token = issue(&expired, 1, "{}");
        }
        assert!(expired.len() <= 1, "expired entries go as new ones come");
        let forever = ledger(8, 64, Duration::MAX, 1);
        let token = issue(&forever, 1, "{}");
        assert_eq!(
            forever.redeem(&token, "t", 0, "{}"),
            Ok(()),
            "a timeout too large to add never expires"
        );
        let small = ledger(2, 64, Duration::from_secs(60), 2);
        let others = issue(&small, 2, "{}");
        let first = issue(&small, 1, "{}");
        let _second = issue(&small, 1, "{}");
        let _third = issue(&small, 1, "{}");
        assert_eq!(small.len(), 3);
        assert_eq!(
            small.redeem(&first, "t", 0, "{}"),
            Err(Redeem::Unknown),
            "the connection's oldest is evicted"
        );
        assert_eq!(
            small.redeem(&others, "t", 0, "{}"),
            Ok(()),
            "another connection's entry stays"
        );
    }

    #[test]
    fn test_ledger_forgets_a_closed_connections_entries() {
        let ledger = ledger(64, 1 << 20, Duration::from_secs(600), 2);
        let kept = issue(&ledger, 2, "{}");
        for connection in 3..103 {
            ledger.open(connection);
            for n in 0..64 {
                let _token = issue(&ledger, connection, &format!("{{\"n\":{n}}}"));
            }
        }
        let closing = issue(&ledger, 1, "{}");
        assert_eq!(ledger.len(), 1 + 100 * 64 + 1);
        for connection in 3..103 {
            ledger.close(connection);
        }
        ledger.close(1);
        assert_eq!(ledger.len(), 1, "only the open connection's entry is left");
        assert_eq!(
            ledger.redeem(&closing, "t", 0, "{}"),
            Err(Redeem::Unknown),
            "forgotten with its connection"
        );
        assert_eq!(
            ledger.issue(1, "t", 0, "{}"),
            Err(Refused::Closed),
            "a call still running when its connection closed adds nothing"
        );
        assert_eq!(ledger.redeem(&kept, "t", 0, "{}"), Ok(()));
        let (entries, places) = ledger.sizes();
        assert_eq!(entries, 0);
        assert!(places <= 2 * SLACK, "{places} places left over");
    }

    #[test]
    fn test_ledger_total_cap_refuses_new_confirmations() {
        let ledger = ledger(2, 3, Duration::from_secs(600), 3);
        let a = issue(&ledger, 1, "{}");
        let _b = issue(&ledger, 1, "{}");
        let _c = issue(&ledger, 2, "{}");
        assert_eq!(ledger.issue(3, "t", 0, "{}"), Err(Refused::Full));
        assert_eq!(ledger.issue(2, "t", 0, "{}"), Err(Refused::Full));
        assert_eq!(ledger.len(), 3);
        // A connection at its own quota replaces its oldest instead.
        assert!(ledger.issue(1, "t", 0, "{}").is_ok());
        assert_eq!(ledger.redeem(&a, "t", 0, "{}"), Err(Redeem::Unknown));
        assert_eq!(ledger.len(), 3);
        // Room again once one is used, or its connection closes.
        ledger.close(2);
        assert!(ledger.issue(3, "t", 0, "{}").is_ok());
        assert_eq!(ledger.issue(3, "t", 0, "{}"), Err(Refused::Full));
    }

    #[test]
    fn test_ledger_bookkeeping_stays_bounded() {
        // Issued and redeemed in a loop, or evicted by the quota: the issue
        // orders keep no more than a constant beyond the live entries.
        let ledger = ledger(4, 1 << 20, Duration::from_secs(600), 2);
        for n in 0..10_000 {
            let token = issue(&ledger, 1, "{}");
            assert_eq!(ledger.redeem(&token, "t", 0, "{}"), Ok(()), "round {n}");
            let _evicted_later = issue(&ledger, 2, "{}");
        }
        let (entries, places) = ledger.sizes();
        assert_eq!(entries, 4);
        assert!(places <= 3 * (entries * 2 + SLACK), "{places} places");
    }

    #[test]
    fn test_ledger_cost_does_not_grow_with_its_size() {
        // The same work (issue and redeem a confirmation, 2,000 times) on an
        // empty ledger and on one holding 20,000 entries from 313 other
        // connections. With a scan of every entry per operation the second
        // takes hundreds of times longer; with the index, about as long.
        // The bound is coarse so a loaded machine cannot fail it.
        fn work(ledger: &Ledger) -> Duration {
            let start = Instant::now();
            for _ in 0..2000 {
                let token = issue(ledger, 1, "{}");
                assert_eq!(ledger.redeem(&token, "t", 0, "{}"), Ok(()));
            }
            start.elapsed()
        }
        let best = |ledger: &Ledger| (0..3).map(|_| work(ledger)).min().unwrap_or_default();
        let empty = ledger(64, 1 << 20, Duration::from_secs(600), 1);
        let full = ledger(64, 1 << 20, Duration::from_secs(600), 1);
        for connection in 2..=314 {
            full.open(connection);
            for n in 0..64 {
                if full.len() < 20_000 {
                    let _token = issue(&full, connection, &format!("{n}"));
                }
            }
        }
        assert_eq!(full.len(), 20_000);
        let (small, large) = (best(&empty), best(&full));
        assert!(
            large < small * 20 + Duration::from_millis(100),
            "{large:?} with 20,000 entries against {small:?} with none"
        );
    }

    #[test]
    fn test_tokens_compare_whole() {
        let token = token();
        assert!(same(&token, &token.clone()));
        let mut last = token.clone().into_bytes();
        if let Some(byte) = last.last_mut() {
            *byte ^= 1;
        }
        let last = String::from_utf8(last).unwrap_or_default();
        assert!(!same(&token, &last), "a difference in the last byte counts");
        assert!(!same(&token, &token[..token.len() - 1]));
        assert!(!same(&token, ""));
    }

    #[test]
    fn test_tokens_do_not_repeat() {
        let mut seen: Vec<String> = (0..1000).map(|_| token()).collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 1000);
    }
}
