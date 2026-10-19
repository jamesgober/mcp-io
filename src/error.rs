//! The crate's error types.

use core::fmt;
use std::io;

use crate::schema::SchemaError;

/// Why serving, or a send from a tool handler, failed.
///
/// Malformed or hostile input is never an `Error`: it is answered on the
/// wire, and serving goes on.
///
/// # Examples
///
/// ```
/// use mcp_io::Error;
///
/// let err = Error::Cancelled;
/// assert_eq!(err.to_string(), "the request was cancelled; nothing more may be sent for it");
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The JSON-RPC connection failed: the transport broke, the input lost
    /// its framing, or the connection is closed. See [`rpc_io::Error`].
    Rpc(rpc_io::Error),
    /// Starting a worker thread or the output writer failed.
    Io(io::Error),
    /// The request was cancelled (or the connection closed); MCP forbids
    /// sending anything more for it, so nothing was sent. Stop working.
    Cancelled,
    /// A progress report did not increase (MCP: progress "MUST increase with
    /// each notification"); nothing was sent.
    ProgressNotIncreasing {
        /// The last value reported.
        last: f64,
        /// The value refused.
        next: f64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(error) => write!(f, "the connection failed: {error}"),
            Self::Io(error) => write!(f, "could not start a server thread: {error}"),
            Self::Cancelled => {
                f.write_str("the request was cancelled; nothing more may be sent for it")
            }
            Self::ProgressNotIncreasing { last, next } => {
                write!(
                    f,
                    "progress must increase with every report ({next} does not exceed {last})"
                )
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rpc(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Cancelled | Self::ProgressNotIncreasing { .. } => None,
        }
    }
}

impl From<rpc_io::Error> for Error {
    fn from(error: rpc_io::Error) -> Self {
        Self::Rpc(error)
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Why a tool could not be registered.
///
/// # Examples
///
/// ```
/// use mcp_io::{RegisterError, Server, Tier, Tool, ToolResult, Implementation};
///
/// let err = Server::new(Implementation::new("demo", "1.0.0"))
///     .tool(Tool::new("bad name", Tier::ReadOnly), |_| Ok(ToolResult::text("")))
///     .err();
/// assert!(matches!(err, Some(RegisterError::InvalidName { .. })));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegisterError {
    /// The name breaks MCP's rule (1 to 128 of ASCII letters, digits, `_`,
    /// `-`, `.`). Rename the tool.
    InvalidName {
        /// The name (clipped).
        name: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A tool with this name is registered already. Remove it first, or use
    /// another name.
    Duplicate {
        /// The name.
        name: String,
    },
    /// The input schema's root is not `"type": "object"`, which MCP requires
    /// (arguments are always an object).
    InputSchemaNotObject {
        /// The tool.
        tool: String,
    },
    /// The input or output schema does not compile. See [`SchemaError`].
    Schema {
        /// The tool.
        tool: String,
        /// `"inputSchema"` or `"outputSchema"`.
        which: &'static str,
        /// Why.
        error: SchemaError,
    },
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name, reason } => write!(f, "the tool name {name:?} {reason}"),
            Self::Duplicate { name } => write!(f, "a tool named {name:?} is already registered"),
            Self::InputSchemaNotObject { tool } => {
                write!(
                    f,
                    "the input schema of {tool:?} must have \"type\": \"object\" at its root"
                )
            }
            Self::Schema { tool, which, error } => {
                write!(f, "the {which} of {tool:?} is not usable: {error}")
            }
        }
    }
}

impl std::error::Error for RegisterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Schema { error, .. } => Some(error),
            Self::InvalidName { .. }
            | Self::Duplicate { .. }
            | Self::InputSchemaNotObject { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;
    use crate::schema::SchemaErrorKind;

    #[test]
    fn test_error_display_and_sources() {
        let io = Error::from(io::Error::other("no threads"));
        assert_eq!(
            io.to_string(),
            "could not start a server thread: no threads"
        );
        assert!(io.source().is_some());
        let rpc = Error::from(rpc_io::Error::Closed);
        assert!(rpc.to_string().starts_with("the connection failed: "));
        assert!(rpc.source().is_some());
        let progress = Error::ProgressNotIncreasing {
            last: 2.0,
            next: 1.5,
        };
        assert_eq!(
            progress.to_string(),
            "progress must increase with every report (1.5 does not exceed 2)"
        );
        assert!(progress.source().is_none() && Error::Cancelled.source().is_none());
    }

    #[test]
    fn test_register_error_display_and_sources() {
        let schema = RegisterError::Schema {
            tool: "t".into(),
            which: "inputSchema",
            error: SchemaError::new("/x".into(), SchemaErrorKind::TooDeep { limit: 1 }),
        };
        assert_eq!(
            schema.to_string(),
            "the inputSchema of \"t\" is not usable: invalid schema at /x: subschemas nest deeper than the limit of 1"
        );
        assert!(schema.source().is_some());
        assert_eq!(
            RegisterError::Duplicate { name: "t".into() }.to_string(),
            "a tool named \"t\" is already registered"
        );
        assert!(
            RegisterError::InputSchemaNotObject { tool: "t".into() }
                .source()
                .is_none()
        );
        assert!(
            RegisterError::InvalidName {
                name: "a b".into(),
                reason: "is bad"
            }
            .to_string()
            .contains("\"a b\" is bad")
        );
    }
}
