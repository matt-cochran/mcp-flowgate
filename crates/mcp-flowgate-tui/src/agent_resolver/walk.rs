//! Specificity walk + Chain-of-Responsibility resolver.
//!
//! Walk order, per the locked design:
//!
//! 1. `<affinity>-<tier>` (exact match)
//! 2. `<affinity>` (affinity wins tiebreaker)
//! 3. `<tier>`
//! 4. `default`
//!
//! When `strict_specificity: true` is set on the file, step 1's miss
//! short-circuits the whole walk → `AgentResolutionExhausted` (FMECA U1
//! poka-yoke).
//!
//! Per-list CoR is the caller's responsibility (they own the I/O); see
//! `try_next` for the contract.

use std::borrow::Cow;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use crate::agent_resolver::classify::FailureClass;
use crate::agent_resolver::config::{Affinity, AgentsFile, Binding, OverrideKey, Tier};

// ── delegate (workflow-side reference) ──────────────────────────────────────

/// The parsed `delegate:` field from a workflow state. At least one of
/// `affinity` / `tier` is `Some` (a "default-only" delegate makes no
/// sense — workflows that don't want to delegate just omit the field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delegate {
    pub affinity: Option<Affinity>,
    pub tier: Option<Tier>,
}

#[derive(Debug, thiserror::Error)]
pub enum DelegateParseError {
    #[error("delegate string is empty")]
    Empty,
    #[error(
        "delegate `{0}` does not parse as <affinity> | <tier> | <affinity>-<tier>; \
         affinity ∈ {{coding, reasoning, prose, web-search, recon}}, \
         tier ∈ {{frontier, standard, commoditized}}"
    )]
    Unknown(String),
}

impl Delegate {
    /// Parse forms: `coding-frontier`, `coding`, `frontier`. Empty input
    /// returns `Empty` so the caller can distinguish "no delegate" from
    /// "garbage delegate."
    pub fn parse(raw: &str) -> Result<Self, DelegateParseError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(DelegateParseError::Empty);
        }
        if let Some(idx) = raw.rfind('-') {
            let (left, right) = (&raw[..idx], &raw[idx + 1..]);
            if let (Ok(a), Ok(t)) = (Affinity::from_str(left), Tier::from_str(right)) {
                return Ok(Delegate {
                    affinity: Some(a),
                    tier: Some(t),
                });
            }
        }
        if let Ok(a) = Affinity::from_str(raw) {
            return Ok(Delegate {
                affinity: Some(a),
                tier: None,
            });
        }
        if let Ok(t) = Tier::from_str(raw) {
            return Ok(Delegate {
                affinity: None,
                tier: Some(t),
            });
        }
        Err(DelegateParseError::Unknown(raw.to_string()))
    }
}

impl fmt::Display for Delegate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.affinity, self.tier) {
            (Some(a), Some(t)) => write!(f, "{a}-{t}"),
            (Some(a), None) => write!(f, "{a}"),
            (None, Some(t)) => write!(f, "{t}"),
            (None, None) => f.write_str("(empty)"),
        }
    }
}

// ── config source ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum ConfigSource {
    Project(PathBuf),
    User(PathBuf),
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigSource::Project(p) => write!(f, "project ({})", p.display()),
            ConfigSource::User(p) => write!(f, "user ({})", p.display()),
        }
    }
}

// ── resolver ────────────────────────────────────────────────────────────────

/// Resolves a `Delegate` to a list of `Binding`s to try in order.
#[derive(Debug, Clone)]
pub struct Resolver {
    file: AgentsFile,
    source: ConfigSource,
}

impl Resolver {
    pub fn from_loaded(file: AgentsFile, source: ConfigSource) -> Self {
        Self { file, source }
    }

    pub fn source(&self) -> &ConfigSource {
        &self.source
    }

    pub fn file(&self) -> &AgentsFile {
        &self.file
    }

    /// Returns the binding list the resolver chose for `delegate`, plus
    /// the level it was found at (for the `AGENT_RESOLVER_WALK` audit
    /// event the caller emits). `AgentResolutionExhausted` if no level
    /// matched.
    ///
    /// Honors `strict_specificity` on the file: when set, a delegate
    /// that asked for `<affinity>-<tier>` must be matched by an exact
    /// key — otherwise this returns `AgentResolutionExhausted` with the
    /// strict-mode marker in `walked_levels`.
    pub fn walk(
        &self,
        delegate: &Delegate,
    ) -> Result<(Cow<'_, [Binding]>, String), AgentResolutionExhausted> {
        let mut walked = Vec::new();
        let strict = self.file.strict_specificity;
        let asks_full = delegate.affinity.is_some() && delegate.tier.is_some();
        let mut first_iteration = true;

        for (key, label) in candidate_keys(delegate) {
            if let Some(bindings) = self.file.overrides.get(&key) {
                walked.push(format!("{label} (matched)"));
                return Ok((Cow::Borrowed(bindings.as_slice()), label));
            }
            if first_iteration && strict && asks_full {
                // Strict mode + full delegate (affinity-tier) + first
                // (exact-match) key missed → abort. Don't walk further.
                walked.push(format!("{label} [strict: not found]"));
                return Err(AgentResolutionExhausted {
                    delegate: delegate.to_string(),
                    walked_levels: walked,
                    attempts: Vec::new(),
                });
            }
            walked.push(format!("{label} (not found)"));
            first_iteration = false;
        }
        if self.file.default.is_empty() {
            walked.push("default (empty)".to_string());
            return Err(AgentResolutionExhausted {
                delegate: delegate.to_string(),
                walked_levels: walked,
                attempts: Vec::new(),
            });
        }
        walked.push("default (matched)".to_string());
        Ok((Cow::Borrowed(self.file.default.as_slice()), "default".to_string()))
    }

    /// Pick the next binding to try given prior failures. Walks the
    /// list, skipping indices that already failed; returns the first
    /// untried binding OR a structured exhaustion error.
    ///
    /// Defense-in-depth: if any entry in `prior_failures` is a
    /// non-infrastructure (content) class, surface immediately as
    /// `AgentResolutionExhausted` rather than advancing. Callers are
    /// expected to short-circuit on content failures before re-entering
    /// `try_next`, but the check here prevents the "no silent fallback"
    /// invariant from depending on caller discipline alone (FMECA R1).
    pub fn try_next<'a>(
        &self,
        delegate: &Delegate,
        bindings: &'a [Binding],
        prior_failures: &[(usize, FailureClass, String)],
    ) -> Result<(usize, &'a Binding), AgentResolutionExhausted> {
        let has_content_failure = prior_failures
            .iter()
            .any(|(_, class, _)| !class.is_infrastructure());
        if !has_content_failure {
            let next_idx = prior_failures.iter().map(|(i, _, _)| *i + 1).max().unwrap_or(0);
            if let Some(b) = bindings.get(next_idx) {
                return Ok((next_idx, b));
            }
        }
        let attempts: Vec<AttemptRecord> = prior_failures
            .iter()
            .map(|(i, class, detail)| AttemptRecord {
                binding: bindings[*i].clone(),
                class: *class,
                detail: detail.clone(),
            })
            .collect();
        Err(AgentResolutionExhausted {
            delegate: delegate.to_string(),
            walked_levels: vec!["(see attempts)".to_string()],
            attempts,
        })
    }
}

/// Walk order for the specificity match: full, affinity-only, tier-only.
/// Tie-break: affinity beats tier (so a delegate `coding-frontier` with
/// both `coding` and `frontier` defined picks `coding`).
fn candidate_keys(delegate: &Delegate) -> Vec<(OverrideKey, String)> {
    let mut out = Vec::new();
    if let (Some(a), Some(t)) = (delegate.affinity, delegate.tier) {
        out.push((
            OverrideKey {
                affinity: Some(a),
                tier: Some(t),
            },
            format!("{a}-{t}"),
        ));
    }
    if let Some(a) = delegate.affinity {
        out.push((
            OverrideKey {
                affinity: Some(a),
                tier: None,
            },
            format!("{a}"),
        ));
    }
    if let Some(t) = delegate.tier {
        out.push((
            OverrideKey {
                affinity: None,
                tier: Some(t),
            },
            format!("{t}"),
        ));
    }
    out
}

// ── resolution exhaustion ──────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
#[error(
    "agent resolution exhausted for delegate `{delegate}`. Walked: {walked_levels:?}. \
     Attempts: {attempts:?}"
)]
pub struct AgentResolutionExhausted {
    pub delegate: String,
    pub walked_levels: Vec<String>,
    pub attempts: Vec<AttemptRecord>,
}

#[derive(Debug, Clone)]
pub struct AttemptRecord {
    pub binding: Binding,
    pub class: FailureClass,
    pub detail: String,
}
