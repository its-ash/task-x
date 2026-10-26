//! Structured error taxonomy for tool execution and agent operations. Every failure is classified
//! so the agent can react correctly instead of treating all failures the same: the model gets the
//! severity and a recovery hint, hooks get the severity, and only transient failures are retried.

use crate::tools::{TOOL_ATTEMPTS, ToolError, ToolErrorKind};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Severity determines whether the call is retried, the model changes its approach, or it stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Transient failure: the same call may succeed. Read-only calls are retried automatically.
    Retryable,
    /// Fails again as it is. Fix the input or change the approach.
    Permanent,
    /// The model cannot proceed this way until the user or the policy allows it.
    Blocked,
}

/// The root cause category for any error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "detail")]
pub enum ErrorKind {
    /// Missing resource (file not found, symbol not defined, etc.)
    NotFound(String),
    /// Permission / policy refused the action
    PolicyViolation(String),
    /// Invalid arguments, or the workspace does not match them (no match, stale file, conflict)
    ValidationError(String),
    /// A momentary failure or timeout
    ServiceError(String),
    /// I/O failure writing files, reading directories, running commands
    IOError(String),
    /// The tool ran and failed
    Internal(String),
    /// Operation exceeded its token / turn budget
    BudgetExhausted(String),
    /// User cancelled the operation
    Cancelled,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(_) => write!(f, "not found"),
            Self::PolicyViolation(_) => write!(f, "policy violation"),
            Self::ValidationError(_) => write!(f, "validation error"),
            Self::ServiceError(_) => write!(f, "temporary failure"),
            Self::IOError(_) => write!(f, "I/O error"),
            Self::Internal(_) => write!(f, "failed"),
            Self::BudgetExhausted(_) => write!(f, "budget exhausted"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// A single structured error produced by a tool or by the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Error {
    /// Which category of failure occurred.
    pub kind: ErrorKind,
    /// Severity guiding the reaction.
    pub severity: Severity,
    /// Human-readable message.
    pub message: String,
    /// Optional suggestion for how to recover.
    pub hint: Option<String>,
    /// Tool name if this came from tool execution.
    pub tool: Option<String>,
    /// Automatic retries already made for this call.
    pub retry_count: u32,
}

impl Error {
    fn new(kind: ErrorKind, severity: Severity, message: String, hint: Option<&str>, tool: Option<String>) -> Self {
        Self { kind, severity, message, hint: hint.map(str::to_owned), tool, retry_count: 0 }
    }

    pub fn not_found(tool: impl Into<String>, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self::new(
            ErrorKind::NotFound(detail.clone()),
            Severity::Permanent,
            detail,
            Some("try a different path or symbol name, or search for it first"),
            Some(tool.into()),
        )
    }

    pub fn policy_violation(tool: impl Into<String>, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self::new(
            ErrorKind::PolicyViolation(reason.clone()),
            Severity::Blocked,
            reason,
            Some("do not retry this call; adjust the approach or ask the user"),
            Some(tool.into()),
        )
    }

    pub fn validation_error(tool: impl Into<String>, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        let tool = tool.into();
        let hint = match tool.as_str() {
            "read_file" | "read_range" => "check the requested lines against the file's total (read the whole file, or run get_file_metadata for its line count), then retry with a valid range".to_owned(),
            t => format!("fix the arguments for {t}, or read the file again, then retry"),
        };
        Self::new(ErrorKind::ValidationError(detail.clone()), Severity::Permanent, detail, Some(&hint), Some(tool))
    }

    pub fn io_error(tool: impl Into<String>, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self::new(
            ErrorKind::IOError(detail.clone()),
            Severity::Permanent,
            detail,
            Some("check the path, file permissions, or disk space"),
            Some(tool.into()),
        )
    }

    pub fn service_error(tool: impl Into<String>, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self::new(
            ErrorKind::ServiceError(detail.clone()),
            Severity::Retryable,
            detail,
            Some("a temporary failure; the same call may succeed after a short wait"),
            Some(tool.into()),
        )
    }

    /// The token or turn budget ran out. Stops the tool loop; the model writes a summary.
    pub fn budget_exhausted(detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self::new(
            ErrorKind::BudgetExhausted(detail.clone()),
            Severity::Blocked,
            detail,
            Some("stop using tools; summarize what is done, what is still open, and what the user should do next"),
            None,
        )
    }

    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, Severity::Blocked, "The operation was cancelled.".into(), None, None)
    }

    pub fn internal(tool: impl Into<String>, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self::new(
            ErrorKind::Internal(detail.clone()),
            Severity::Permanent,
            detail,
            Some("the same call fails again; change the approach"),
            Some(tool.into()),
        )
    }

    /// Classifies a tool failure.
    pub fn from_tool(tool: &str, e: &ToolError) -> Self {
        use ToolErrorKind as K;
        let m = e.message.clone();
        match e.kind {
            K::NotFound => Self::not_found(tool, m),
            K::Forbidden => Self::policy_violation(tool, m),
            K::InvalidInput | K::Ambiguous | K::NoMatch | K::Stale | K::Conflict => Self::validation_error(tool, m),
            K::Timeout | K::Transient => Self::service_error(tool, m),
            K::Io => Self::io_error(tool, m),
            K::Cancelled => Self { message: m, tool: Some(tool.into()), ..Self::cancelled() },
            K::Failed => Self::internal(tool, m),
        }
    }

    /// The same failure after one more automatic retry.
    pub fn retry(self) -> Self {
        Self { retry_count: self.retry_count + 1, ..self }
    }

    /// Whether the same call may run again automatically (at most `TOOL_ATTEMPTS` runs in total).
    pub fn is_retryable(&self) -> bool {
        self.severity == Severity::Retryable && self.retry_count + 1 < TOOL_ATTEMPTS
    }

    /// The text the model reads for a failed call: severity, kind, message, and the recovery hint.
    pub fn render(&self) -> String {
        let tag = match self.severity {
            Severity::Retryable => "[RETRY]",
            Severity::Permanent => "[FAIL]",
            Severity::Blocked => "[BLOCKED]",
        };
        let head = format!("{tag} [{}] {}", self.kind, self.message);
        match &self.hint {
            Some(h) => format!("{head}\nHint: {h}"),
            None => head,
        }
    }
}

impl std::error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_tool_failures_and_limits_retries() {
        let err = |kind, m: &str| Error::from_tool("read_file", &ToolError::new(kind, m));
        let e = err(ToolErrorKind::Transient, "interrupted");
        assert_eq!(e.severity, Severity::Retryable);
        assert!(e.is_retryable() && e.clone().retry().is_retryable() && !e.retry().retry().is_retryable(), "three runs at most");
        assert!(!err(ToolErrorKind::Io, "disk full").is_retryable(), "an I/O error is not retried as it is");
        assert_eq!(err(ToolErrorKind::Forbidden, "denied").severity, Severity::Blocked);
        let r = err(ToolErrorKind::NoMatch, "oldText not found").render();
        assert!(r.starts_with("[FAIL] [validation error] oldText not found") && r.contains("\nHint: "), "{r}");
        assert_eq!(Error::cancelled().render(), "[BLOCKED] [cancelled] The operation was cancelled.");
        assert!(Error::budget_exhausted("The token budget was reached.").render().contains("token budget was reached"));
    }
}
