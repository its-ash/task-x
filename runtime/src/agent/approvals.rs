//! The approval list behind a settings screen: every tool (task-x's own and each MCP server's)
//! with what the permission policy decides for it and why, and the edit that sets one tool, or
//! one whole server, to "always allow", "ask", or "never". The lists themselves stay in the
//! client's settings (`allow` and `deny` of the policy); these functions only read and rewrite
//! them, so the screen and the agent can never disagree.

use super::policy::{Decision, Policy, PolicyConfig, tool_matches};
use crate::mcp::McpTool;
use crate::tools::Permission;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// Why a tool gets its decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Why {
    /// An entry of `deny` (in `rule`) refuses it.
    Denied,
    /// An entry of `allow` (in `rule`) lets it run.
    Allowed,
    /// The user chose "allow for this session" when it asked.
    Session,
    /// The mode and edit mode decide (no list names it).
    Mode,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolApproval {
    /// The name the lists use: `read_file`, `mcp__chrome-devtools__new_page`.
    pub name: String,
    /// The MCP server, or `None` for task-x's own tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// The tool name inside its server (the name without `mcp__<server>__`).
    pub tool: String,
    pub permission: Permission,
    pub decision: Decision,
    pub why: Why,
    /// The list entry that decided, such as `mcp__chrome-devtools__*`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
}

/// One MCP server: its `pattern` is the entry that covers all its tools.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerApproval {
    pub name: String,
    pub pattern: String,
    pub tools: usize,
    pub allowed: usize,
    pub asks: usize,
    pub denied: usize,
    /// `allow` or `deny` holds `pattern` itself.
    pub allow_listed: bool,
    pub deny_listed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Approvals {
    pub tools: Vec<ToolApproval>,
    pub servers: Vec<ServerApproval>,
}

/// The entry that covers every tool of `server`: `mcp__<server>__*`.
pub fn server_pattern(server: &str) -> String {
    format!("{}*", crate::mcp::qualify(server, ""))
}

fn approval(policy: &Policy, grants: &HashSet<String>, name: String, server: Option<String>, tool: String, permission: Permission) -> ToolApproval {
    let decision = policy.decide(&name, permission);
    let deny_rule = policy.config.deny.iter().find(|d| tool_matches(d, &name));
    let allow_rule = policy.config.allow.iter().find(|a| tool_matches(a, &name));
    let (why, rule) = match (decision, deny_rule, allow_rule) {
        (Decision::Deny, Some(d), _) => (Why::Denied, Some(d.clone())),
        (Decision::Allow, _, Some(a)) => (Why::Allowed, Some(a.clone())),
        (Decision::Allow, _, None) if grants.contains(&name) => (Why::Session, None),
        _ => (Why::Mode, None),
    };
    ToolApproval { name, server, tool, permission, decision, why, rule }
}

/// Every tool and what `config` (with the session's `grants`) decides for it: task-x's own tools
/// first, then the MCP tools by server. MCP tools are checked as network calls, as when they run.
pub fn list(config: &PolicyConfig, grants: &HashSet<String>, mcp: &[McpTool]) -> Approvals {
    let mut policy = Policy::new(config.clone());
    for g in grants {
        policy.grant_session(g);
    }
    let mut tools: Vec<ToolApproval> =
        crate::tools::registry().iter().map(|t| approval(&policy, grants, t.name.to_owned(), None, t.name.to_owned(), t.permission)).collect();
    let mut servers: BTreeMap<String, ServerApproval> = BTreeMap::new();
    for t in mcp {
        let a = approval(&policy, grants, t.qualified.clone(), Some(t.server.clone()), t.name.clone(), Permission::Network);
        let pattern = server_pattern(&t.server);
        let s = servers.entry(t.server.clone()).or_insert_with(|| ServerApproval {
            name: t.server.clone(),
            allow_listed: config.allow.contains(&pattern),
            deny_listed: config.deny.contains(&pattern),
            pattern,
            tools: 0,
            allowed: 0,
            asks: 0,
            denied: 0,
        });
        s.tools += 1;
        match a.decision {
            Decision::Allow => s.allowed += 1,
            Decision::Ask => s.asks += 1,
            Decision::Deny => s.denied += 1,
        }
        tools.push(a);
    }
    Approvals { tools, servers: servers.into_values().collect() }
}

/// What the user picks for a tool or a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Choice {
    /// Never asks.
    Allow,
    /// Follows the mode: asks where the mode asks.
    Ask,
    /// Never runs.
    Deny,
}

/// The lists after a choice, for the client to save in its settings.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Edit {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    /// Another entry that still decides for `name`, so the choice does not show yet: a `deny`
    /// pattern over an allowed tool, or any pattern over a tool set back to ask.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covered_by: Option<String>,
}

/// Sets `name` (a tool, or a pattern such as a server's `mcp__<server>__*`) to `choice`: its own
/// entries leave both lists, and `allow` or `deny` gets it back. Other entries are kept as they are.
pub fn set(config: &PolicyConfig, name: &str, choice: Choice) -> Result<Edit, String> {
    let name = name.trim();
    if name.is_empty() || name == "*" {
        return Err("give a tool name or a pattern such as mcp__<server>__*".into());
    }
    let mut allow: Vec<String> = config.allow.iter().filter(|a| a.as_str() != name).cloned().collect();
    let mut deny: Vec<String> = config.deny.iter().filter(|d| d.as_str() != name).cloned().collect();
    let covered_by = match choice {
        Choice::Allow => {
            allow.push(name.to_owned());
            deny.iter().find(|d| tool_matches(d, name)).cloned()
        }
        Choice::Deny => {
            deny.push(name.to_owned());
            None
        }
        Choice::Ask => deny.iter().chain(&allow).find(|e| tool_matches(e, name)).cloned(),
    };
    Ok(Edit { allow, deny, covered_by })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::policy::{EditMode, Mode};
    use serde_json::json;

    fn mcp(server: &str, name: &str) -> McpTool {
        McpTool { server: server.into(), name: name.into(), qualified: crate::mcp::qualify(server, name), description: String::new(), input_schema: json!({}) }
    }

    fn safe(allow: &[&str], deny: &[&str]) -> PolicyConfig {
        PolicyConfig {
            mode: Mode::Autonomous,
            edit_mode: Some(EditMode::Safe),
            sandboxed: Some(true),
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn lists_every_tool_with_its_decision_and_reason() {
        let tools = [mcp("chrome-devtools", "new_page"), mcp("chrome-devtools", "evaluate_script"), mcp("figma", "get")];
        let grants = HashSet::from(["mcp__figma__get".to_string()]);
        let a = list(&safe(&["mcp__chrome-devtools__*"], &["mcp__chrome-devtools__evaluate_script", "web_fetch"]), &grants, &tools);
        let get = |n: &str| a.tools.iter().find(|t| t.name == n).unwrap();
        assert_eq!((get("read_file").decision, get("read_file").why, get("read_file").server.clone()), (Decision::Allow, Why::Mode, None));
        assert_eq!((get("web_fetch").decision, get("web_fetch").why, get("web_fetch").rule.as_deref()), (Decision::Deny, Why::Denied, Some("web_fetch")));
        let np = get("mcp__chrome-devtools__new_page");
        assert_eq!((np.decision, np.why, np.rule.as_deref(), np.tool.as_str()), (Decision::Allow, Why::Allowed, Some("mcp__chrome-devtools__*"), "new_page"));
        assert_eq!(get("mcp__chrome-devtools__evaluate_script").decision, Decision::Deny, "the deny entry wins over the server pattern");
        assert_eq!((get("mcp__figma__get").decision, get("mcp__figma__get").why), (Decision::Allow, Why::Session));
        assert_eq!(a.servers.len(), 2);
        let chrome = &a.servers[0];
        assert_eq!((chrome.name.as_str(), chrome.pattern.as_str()), ("chrome-devtools", "mcp__chrome-devtools__*"));
        assert_eq!((chrome.tools, chrome.allowed, chrome.asks, chrome.denied, chrome.allow_listed), (2, 1, 0, 1, true));
        let figma = list(&safe(&[], &[]), &HashSet::new(), &tools);
        assert_eq!(figma.tools.iter().find(|t| t.name == "mcp__figma__get").unwrap().decision, Decision::Ask, "safe mode asks before MCP tools");
    }

    #[test]
    fn choices_rewrite_the_lists() {
        let c = safe(&["run_tests", "mcp__chrome-devtools__*"], &["mcp__chrome-devtools__evaluate_script"]);
        let e = set(&c, "mcp__chrome-devtools__evaluate_script", Choice::Allow).unwrap();
        assert!(e.allow.contains(&"mcp__chrome-devtools__evaluate_script".to_string()) && e.deny.is_empty() && e.covered_by.is_none());
        let e = set(&c, "mcp__chrome-devtools__*", Choice::Ask).unwrap();
        assert_eq!((e.allow, e.covered_by), (vec!["run_tests".to_string()], None), "the server goes back to asking");
        let e = set(&c, "mcp__chrome-devtools__new_page", Choice::Ask).unwrap();
        assert_eq!(e.covered_by.as_deref(), Some("mcp__chrome-devtools__*"), "the server pattern still allows it");
        let e = set(&safe(&[], &["mcp__x__*"]), "mcp__x__a", Choice::Allow).unwrap();
        assert_eq!(e.covered_by.as_deref(), Some("mcp__x__*"), "a deny pattern still refuses it");
        let e = set(&c, "run_tests", Choice::Deny).unwrap();
        assert!(!e.allow.contains(&"run_tests".to_string()) && e.deny.contains(&"run_tests".to_string()));
        assert!(set(&c, " * ", Choice::Allow).is_err() && set(&c, "", Choice::Deny).is_err());
        assert_eq!(server_pattern("chrome devtools"), "mcp__chrome_devtools__*", "the pattern uses the cleaned server name");
    }
}
