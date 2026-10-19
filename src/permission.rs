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
//!    The question shows the call's arguments in full (canonical JSON,
//!    non-ASCII escaped), never a part of them; a call too large to show is
//!    refused. Then:
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
//! the tool's registration, a keyed 128-bit digest of the call's canonical
//! arguments, and an expiry. Nothing in the token itself is trusted. A
//! tampered or replayed token names no entry and is refused (tokens are
//! compared in constant time); an entry is
//! removed when it is presented, so each confirmation runs at most one call;
//! entries expire (10 minutes by default) and are capped per connection. The
//! digest's keys are random per server, so a client cannot search for other
//! arguments with the same digest. On stdio
//! there is no authenticated principal to bind; the token is the capability,
//! and it is sent only to the client that made the call. Tokens come from
//! the standard library's randomly seeded SipHash keys (seeded from the
//! operating system's entropy), not from a dedicated CSPRNG API.

use std::{
    borrow::Cow,
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
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
}

/// The host's say over every tool call, consulted after validation and
/// before the handler (see the [module documentation](crate::permission)).
///
/// Implement it for authorization, per-workspace switches, or rate limits
/// (MCP: servers "MUST rate limit tool invocations"; [`Decision::Deny`]
/// with a retry hint is how). A closure `Fn(&PermissionRequest) -> Decision`
/// is a policy. A policy that panics denies the call.
///
/// It is consulted once per call. On 2026-07-28 a confirmed call arrives
/// twice (the call, then its retry with the `requestState` and the user's
/// answer); the policy decides the first, and the retry is not decided
/// again: it runs only if it redeems the confirmation the first one issued.
///
/// # Examples
///
/// ```
/// use std::sync::Mutex;
/// use mcp_io::{Decision, PermissionRequest, Policy, Tier};
///
/// /// Read-only tools run; at most 100 other calls are allowed, each confirmed
/// /// (one asked for and declined counts too).
/// struct Budgeted { left: Mutex<u32> }
///
/// impl Policy for Budgeted {
///     fn decide(&self, request: &PermissionRequest<'_>) -> Decision {
///         if request.tier() == Tier::ReadOnly {
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
    /// The connection it was issued on, for its quota (it may be redeemed on
    /// any connection: MCP does not tie requests to connections).
    connection: u64,
    binding: Binding,
    /// `None`: a timeout too large to add to the clock; it never expires.
    expires: Option<Instant>,
}

/// What a confirmation is bound to: the tool's registration and a keyed
/// digest of the call's canonical arguments. The digest, not the text, is
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

/// The server's outstanding confirmations, shared by every connection.
pub(crate) struct Ledger {
    pending: Mutex<Vec<Pending>>,
    /// Outstanding confirmations per connection.
    capacity: usize,
    ttl: Duration,
    /// The digest's keys: random per ledger, so a client cannot search for
    /// other arguments with the same digest.
    keys: [RandomState; 2],
}

impl Ledger {
    pub(crate) fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
            capacity: capacity.max(1),
            ttl,
            keys: [RandomState::new(), RandomState::new()],
        }
    }

    fn bind(&self, tool: &str, generation: u64, arguments: &str) -> Binding {
        let digest = self.keys.each_ref().map(|key| {
            let mut hasher = key.build_hasher();
            hasher.write(arguments.as_bytes());
            hasher.finish()
        });
        Binding {
            tool: tool.into(),
            generation,
            digest,
            length: arguments.len(),
        }
    }

    /// Records a confirmation for registration `generation` of `tool` with
    /// `arguments` (canonical text), issued on `connection`, and returns its
    /// token. Expired entries go first; past the connection's quota, its
    /// oldest entry goes, so one connection cannot evict another's.
    pub(crate) fn issue(
        &self,
        connection: u64,
        tool: &str,
        generation: u64,
        arguments: &str,
    ) -> String {
        let token = token();
        let binding = self.bind(tool, generation, arguments);
        let now = Instant::now();
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        pending.retain(|p| p.expires.is_none_or(|at| at > now));
        let mine = pending
            .iter()
            .filter(|p| p.connection == connection)
            .count();
        if mine >= self.capacity {
            // Entries share one TTL, so this connection's first is its oldest.
            if let Some(oldest) = pending.iter().position(|p| p.connection == connection) {
                let _evicted = pending.remove(oldest);
            }
        }
        pending.push(Pending {
            token: token.clone(),
            connection,
            binding,
            expires: now.checked_add(self.ttl),
        });
        token
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
        let binding = self.bind(tool, generation, arguments);
        let now = Instant::now();
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(at) = pending.iter().position(|p| same(&p.token, token)) else {
            return Err(Redeem::Unknown);
        };
        let entry = pending.remove(at);
        if entry.expires.is_some_and(|at| at <= now) {
            return Err(Redeem::Unknown);
        }
        if entry.binding != binding {
            return Err(Redeem::Mismatch);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
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

/// A fresh, unpredictable confirmation token.
fn token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut out = String::from("mcp-io.confirm.");
    for half in 0..2u8 {
        // Every `RandomState` carries its own SipHash keys, derived from
        // OS-seeded per-thread keys; the output is unpredictable without them.
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(count);
        hasher.write_u128(nanos);
        hasher.write_u8(half);
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
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
        };
        assert_eq!(policy.decide(&r), Decision::deny("never"));
    }

    #[test]
    fn test_ledger_tokens_are_single_use_and_bound_to_the_call() {
        let ledger = Ledger::new(8, Duration::from_secs(60));
        let token = ledger.issue(1, "ship", 0, "{}");
        assert!(token.starts_with("mcp-io.confirm.") && token.len() == 15 + 32);
        assert_eq!(ledger.redeem(&token, "ship", 0, "{}"), Ok(()));
        assert_eq!(
            ledger.redeem(&token, "ship", 0, "{}"),
            Err(Redeem::Unknown),
            "replay"
        );
        let token = ledger.issue(1, "ship", 0, "{\"v\":1}");
        assert_eq!(
            ledger.redeem(&token, "ship", 0, "{\"v\":2}"),
            Err(Redeem::Mismatch)
        );
        assert_eq!(
            ledger.redeem(&token, "ship", 0, "{\"v\":1}"),
            Err(Redeem::Unknown),
            "a mismatch burns the token"
        );
        let token = ledger.issue(1, "ship", 0, "{}");
        assert_eq!(
            ledger.redeem(&token, "other", 0, "{}"),
            Err(Redeem::Mismatch)
        );
        let token = ledger.issue(1, "ship", 0, "{}");
        assert_eq!(
            ledger.redeem(&token, "ship", 1, "{}"),
            Err(Redeem::Mismatch),
            "a tool registered again under the same name"
        );
        // Redeemable from another connection: MCP ties nothing to one.
        let token = ledger.issue(1, "ship", 0, "{}");
        assert_eq!(ledger.redeem(&token, "ship", 0, "{}"), Ok(()));
        assert_eq!(
            ledger.redeem("mcp-io.confirm.forged", "ship", 0, "{}"),
            Err(Redeem::Unknown)
        );
    }

    #[test]
    fn test_ledger_keeps_a_digest_not_the_arguments() {
        let ledger = Ledger::new(8, Duration::from_secs(60));
        let big = "x".repeat(1 << 20);
        let token = ledger.issue(1, "ship", 0, &big);
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
        let expired = Ledger::new(8, Duration::ZERO);
        let token = expired.issue(1, "t", 0, "{}");
        assert_eq!(expired.redeem(&token, "t", 0, "{}"), Err(Redeem::Unknown));
        let forever = Ledger::new(8, Duration::MAX);
        let token = forever.issue(1, "t", 0, "{}");
        assert_eq!(
            forever.redeem(&token, "t", 0, "{}"),
            Ok(()),
            "a timeout too large to add never expires"
        );
        let small = Ledger::new(2, Duration::from_secs(60));
        let others = small.issue(2, "t", 0, "{}");
        let first = small.issue(1, "t", 0, "{}");
        let _second = small.issue(1, "t", 0, "{}");
        let _third = small.issue(1, "t", 0, "{}");
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
