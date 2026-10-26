//! Cost estimates in US dollars. Prices are per million tokens and match a model id by prefix.
//! User rules (`RunParams::prices`) come first; the built-in table covers Anthropic first-party
//! rates (cached 2026-10-06). Local providers cost nothing. Other models stay unpriced until the
//! user adds a rule, and the summary says so instead of guessing.

use crate::provider::{ProviderConfig, ProviderKind, Usage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PriceRule {
    /// Model id prefix, for example "gpt-5" or "claude-opus-5-5".
    pub model: String,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

/// Most specific prefix first: "claude-opus-5-5" must win over "claude-opus-5".
const BUILTIN: &[(&str, f64, f64)] = &[
    ("claude-fable-5-1", 10.0, 50.0),
    ("claude-fable-5", 10.0, 50.0),
    ("claude-mythos-5-1", 10.0, 50.0),
    ("claude-opus-5-5", 4.0, 20.0),
    ("claude-opus-5", 5.0, 25.0),
    ("claude-opus-4-8", 5.0, 25.0),
    ("claude-opus-4-7", 5.0, 25.0),
    ("claude-opus-4-6", 5.0, 25.0),
    ("claude-sonnet-5-5", 2.0, 10.0),
    ("claude-sonnet-5", 2.0, 10.0),
    ("claude-sonnet-4-6", 3.0, 15.0),
    ("claude-haiku-5-5", 0.10, 0.50),
    ("claude-haiku-4-5", 1.0, 5.0),
];

/// Price per million tokens (input, output) for a model, or None when it is not known.
pub fn price(rules: &[PriceRule], provider: &ProviderConfig) -> Option<(f64, f64)> {
    let m = provider.model.as_str();
    let user = rules
        .iter()
        .filter(|r| !r.model.is_empty() && m.starts_with(&r.model) && r.input_per_mtok >= 0.0 && r.output_per_mtok >= 0.0)
        .max_by_key(|r| r.model.len())
        .map(|r| (r.input_per_mtok, r.output_per_mtok));
    user.or_else(|| {
        (provider.kind == ProviderKind::Anthropic)
            .then(|| BUILTIN.iter().find(|(p, ..)| m.starts_with(p)).map(|&(_, i, o)| (i, o)))
            .flatten()
    })
}

/// Dollar cost of one response, or None when the model has no price.
pub fn estimate(rules: &[PriceRule], provider: &ProviderConfig, u: &Usage) -> Option<f64> {
    let (i, o) = price(rules, provider)?;
    Some((u.input_tokens.unwrap_or(0) as f64 * i + u.output_tokens.unwrap_or(0) as f64 * o) / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(kind: ProviderKind, model: &str) -> ProviderConfig {
        ProviderConfig { kind, model: model.into(), base_url: None, api_key: None, timeout_ms: None }
    }

    #[test]
    fn longest_prefix_and_user_rules_win() {
        let u = Usage { input_tokens: Some(1_000_000), output_tokens: Some(100_000) };
        assert_eq!(estimate(&[], &p(ProviderKind::Anthropic, "claude-opus-5-5"), &u), Some(6.0));
        assert_eq!(estimate(&[], &p(ProviderKind::Anthropic, "claude-opus-5"), &u), Some(7.5));
        assert_eq!(estimate(&[], &p(ProviderKind::OpenAi, "gpt-5"), &u), None, "no guessed prices");
        let rules = [
            PriceRule { model: "gpt".into(), input_per_mtok: 9.0, output_per_mtok: 9.0 },
            PriceRule { model: "gpt-5".into(), input_per_mtok: 1.0, output_per_mtok: 10.0 },
        ];
        assert_eq!(estimate(&rules, &p(ProviderKind::OpenAi, "gpt-5-mini"), &u), Some(2.0));
    }
}
