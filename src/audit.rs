//! The audit trail: one record per `tools/call`, for the host to log.

use core::{fmt, time::Duration};

use crate::{Tier, protocol::Era, rpc_io::Id};

/// How a tool call ended.
///
/// # Examples
///
/// ```
/// use mcp_io::Outcome;
///
/// assert_eq!(Outcome::Denied.to_string(), "denied by the policy");
/// assert!(Outcome::Completed { is_error: false }.ran());
/// assert!(!Outcome::InvalidArguments.ran());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// The arguments failed the input schema; the handler did not run.
    InvalidArguments,
    /// The policy denied the call; the handler did not run.
    Denied,
    /// The call needs the user's confirmation, which was requested (an
    /// `InputRequiredResult` was sent, or an elicitation is pending); the
    /// handler has not run.
    ConfirmationRequested,
    /// The user declined, or the confirmation could not be obtained (no
    /// elicitation support, an invalid `requestState`, a timeout); the
    /// handler did not run.
    NotConfirmed,
    /// The handler ran and returned a result (`is_error`: a tool execution
    /// error).
    Completed {
        /// Whether the result was a tool execution error.
        is_error: bool,
    },
    /// The handler ran and failed outside a result: it panicked, returned a
    /// protocol error, or broke its output schema.
    Failed,
    /// The client cancelled the call (or the connection closed) before it
    /// was answered.
    Cancelled,
}

impl Outcome {
    /// Whether the handler ran.
    #[must_use]
    pub fn ran(self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Failed)
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidArguments => "refused: invalid arguments",
            Self::Denied => "denied by the policy",
            Self::ConfirmationRequested => "waiting for the user's confirmation",
            Self::NotConfirmed => "not confirmed",
            Self::Completed { is_error: false } => "completed",
            Self::Completed { is_error: true } => "completed with a tool error",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        })
    }
}

/// One tool call's record: who asked for what, when, and how it ended.
/// Never the arguments or the result, which may be sensitive.
///
/// Passed to the hook set with [`Server::on_audit`](crate::Server::on_audit).
#[derive(Debug)]
pub struct AuditRecord<'a> {
    pub(crate) tool: &'a str,
    pub(crate) tier: Tier,
    pub(crate) id: &'a Id<'static>,
    pub(crate) era: Era,
    pub(crate) client: Option<&'a str>,
    pub(crate) outcome: Outcome,
    pub(crate) elapsed: Duration,
}

impl<'a> AuditRecord<'a> {
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

    /// The request's JSON-RPC id (a correlation id for logs).
    #[must_use]
    pub fn id(&self) -> &'a Id<'static> {
        self.id
    }

    /// The protocol era of the call.
    #[must_use]
    pub fn era(&self) -> Era {
        self.era
    }

    /// The client's self-reported name, if it sent one (unverified).
    #[must_use]
    pub fn client(&self) -> Option<&'a str> {
        self.client
    }

    /// How the call ended.
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// Time from the request's arrival at the worker to its outcome.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outcomes_say_whether_the_handler_ran() {
        let all = [
            Outcome::InvalidArguments,
            Outcome::Denied,
            Outcome::ConfirmationRequested,
            Outcome::NotConfirmed,
            Outcome::Completed { is_error: false },
            Outcome::Completed { is_error: true },
            Outcome::Failed,
            Outcome::Cancelled,
        ];
        let ran: Vec<bool> = all.iter().map(|o| o.ran()).collect();
        assert_eq!(ran, [false, false, false, false, true, true, true, false]);
        for outcome in all {
            assert!(!outcome.to_string().is_empty());
        }
    }

    #[test]
    fn test_record_accessors() {
        let id = Id::from(3);
        let record = AuditRecord {
            tool: "t",
            tier: Tier::Publish,
            id: &id,
            era: Era::Modern,
            client: Some("c"),
            outcome: Outcome::Denied,
            elapsed: Duration::from_millis(2),
        };
        assert_eq!(
            (record.tool(), record.tier(), record.id(), record.era()),
            ("t", Tier::Publish, &id, Era::Modern)
        );
        assert_eq!(
            (record.client(), record.outcome(), record.elapsed()),
            (Some("c"), Outcome::Denied, Duration::from_millis(2))
        );
    }
}
