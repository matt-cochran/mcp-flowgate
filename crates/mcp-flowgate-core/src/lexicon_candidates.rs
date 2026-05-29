//! SPEC §30.10.10.4 — Candidate ranking for `SUBJECT_NEEDS_DEFINITION`.
//!
//! When a workflow encounters a placeholder subject, the runtime asks: "what
//! lexicon entries might be the one the author meant?" This module walks the
//! *current bounded context* in the merged lexicon and scores each entry across
//! three tiers:
//!
//! - **Tier 1 — exact canonical**: `entry.term == unknown_subject`
//! - **Tier 2 — exact alias**: any alias of the entry matches exactly
//! - **Tier 4 — Levenshtein fuzzy**: edit distance ≤ 1 (close) or ≤ 2 (loose)
//!   against the canonical term or any alias
//!
//! Results are sorted: exact → alias → fuzzy_close → fuzzy_loose, then by
//! distance ascending within each tier. The top 5 are returned.
//!
//! Tier 3 (semantic similarity) is deferred to Task 3.9.

use serde_json::{json, Map, Value};

/// Sort priority for each match kind (lower = higher priority in the ranking).
const PRIORITY_EXACT: u8 = 0;
const PRIORITY_ALIAS: u8 = 1;
const PRIORITY_FUZZY_CLOSE: u8 = 2;
const PRIORITY_FUZZY_LOOSE: u8 = 3;

/// A single candidate entry returned in the `candidates` array of
/// `SUBJECT_NEEDS_DEFINITION`.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The canonical term name of the lexicon entry (never the alias).
    pub term: String,
    /// Edit distance; 0.0 for exact matches.
    pub distance: f32,
    /// One of: `"exact"`, `"alias"`, `"fuzzy_close"`, `"fuzzy_loose"`.
    pub match_kind: &'static str,
    /// First 100 chars of `definition_short` for the entry.
    pub definition_preview: String,
}

impl Candidate {
    fn priority(&self) -> u8 {
        match self.match_kind {
            "exact" => PRIORITY_EXACT,
            "alias" => PRIORITY_ALIAS,
            "fuzzy_close" => PRIORITY_FUZZY_CLOSE,
            _ => PRIORITY_FUZZY_LOOSE,
        }
    }

    /// Convert to the JSON wire shape.
    pub fn to_json(&self) -> Value {
        json!({
            "term": self.term,
            "distance": self.distance,
            "match_kind": self.match_kind,
            "definition_preview": self.definition_preview,
        })
    }
}

/// Compute the Levenshtein (edit) distance between two strings.
///
/// Classic O(m×n) DP — Unicode-aware (operates on chars, not bytes).
/// Returns `usize::MAX` when both strings are empty (degenerate; never
/// reached in practice because empty subjects are rejected upstream).
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();

    // Fast paths.
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }

    // Two-row DP: only need previous and current.
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr: Vec<usize> = vec![0; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1)
                .min(curr[j - 1] + 1)
                .min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}

/// SPEC §30.10.10.4 — rank candidates for an unknown subject.
///
/// `unknown_subject` — the placeholder term to look up.
/// `lexicon_map` — the `_lexiconLibrary` map from the merged workflow definition
///                 (keyed by canonical term, values are lexicon entry objects).
/// `bounded_context` — optional; when `Some`, only entries with a matching
///                      `bounded_context` field (or `""`) are considered.
///
/// Returns at most 5 candidates, sorted by tier then distance.
///
/// Entries are deduplicated by canonical term: only the best match per entry is
/// retained (e.g. if both the canonical and an alias match fuzzy, only the
/// closer one is returned).
pub fn rank_candidates(
    unknown_subject: &str,
    lexicon_map: &Map<String, Value>,
    bounded_context: Option<&str>,
) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = Vec::new();

    for (term, entry) in lexicon_map {
        let entry_obj = match entry.as_object() {
            Some(o) => o,
            None => continue,
        };

        // Skip placeholder entries (PENDING_DEFINITION) — they are not candidates.
        if entry_obj
            .get("state")
            .and_then(Value::as_str)
            == Some("PENDING_DEFINITION")
        {
            continue;
        }

        // Bounded context filter.
        if let Some(filter_ctx) = bounded_context {
            let entry_ctx = entry_obj
                .get("bounded_context")
                .and_then(Value::as_str)
                .unwrap_or("");
            if entry_ctx != filter_ctx {
                continue;
            }
        }

        let definition_short = entry_obj
            .get("definition_short")
            .and_then(Value::as_str)
            .unwrap_or("");
        let preview: String = definition_short.chars().take(100).collect();

        // Build all surface forms for this entry: canonical + all aliases.
        let mut surface_forms: Vec<&str> = vec![term.as_str()];
        if let Some(aliases) = entry_obj.get("aliases").and_then(Value::as_array) {
            for alias_val in aliases {
                if let Some(alias) = alias_val.as_str() {
                    surface_forms.push(alias);
                }
            }
        }

        // Find the best match across all surface forms for this entry.
        let mut best: Option<Candidate> = None;

        for form in &surface_forms {
            let is_canonical = *form == term.as_str();

            // Tier 1 — exact canonical.
            if is_canonical && *form == unknown_subject {
                let c = Candidate {
                    term: term.clone(),
                    distance: 0.0,
                    match_kind: "exact",
                    definition_preview: preview.clone(),
                };
                best = Some(c);
                break; // exact canonical is the best possible match
            }

            // Tier 2 — exact alias.
            if !is_canonical && *form == unknown_subject {
                let c = Candidate {
                    term: term.clone(),
                    distance: 0.0,
                    match_kind: "alias",
                    definition_preview: preview.clone(),
                };
                // Alias exact is better than fuzzy; replace if current best is fuzzy.
                match &best {
                    None => best = Some(c),
                    Some(existing) if existing.priority() > PRIORITY_ALIAS => best = Some(c),
                    _ => {}
                }
                continue;
            }

            // Tier 4 — Levenshtein fuzzy (≤ 2).
            let dist = levenshtein(unknown_subject, form);
            let match_kind: Option<&'static str> = match dist {
                1 => Some("fuzzy_close"),
                2 => Some("fuzzy_loose"),
                _ => None,
            };
            if let Some(kind) = match_kind {
                let c = Candidate {
                    term: term.clone(),
                    distance: dist as f32,
                    match_kind: kind,
                    definition_preview: preview.clone(),
                };
                let c_prio = c.priority();
                match &best {
                    None => best = Some(c),
                    Some(existing) => {
                        // Replace if: lower priority rank, or same rank but closer.
                        if c_prio < existing.priority()
                            || (c_prio == existing.priority() && c.distance < existing.distance)
                        {
                            best = Some(c);
                        }
                    }
                }
            }
        }

        if let Some(c) = best {
            candidates.push(c);
        }
    }

    // Sort: priority tier first, then by distance ascending, then by term
    // alphabetically for determinism.
    candidates.sort_by(|a, b| {
        a.priority()
            .cmp(&b.priority())
            .then(a.distance.partial_cmp(&b.distance).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.term.cmp(&b.term))
    });

    // Keep top 5.
    candidates.truncate(5);
    candidates
}

/// Convenience: extract the `_lexiconLibrary` map from a synthetic definition
/// value (as produced by `FlowgateServer::lexicon_merged_definition`) and call
/// `rank_candidates`. Returns an empty Vec when the library is absent or
/// malformed.
pub fn rank_candidates_from_definition(
    unknown_subject: &str,
    workflow_definition: &Value,
    bounded_context: Option<&str>,
) -> Vec<Candidate> {
    let lib = match workflow_definition
        .get("_lexiconLibrary")
        .and_then(Value::as_object)
    {
        Some(m) => m,
        None => return Vec::new(),
    };
    rank_candidates(unknown_subject, lib, bounded_context)
}

/// Convert a slice of `Candidate`s to the JSON array used in the MCP response.
pub fn candidates_to_json(candidates: &[Candidate]) -> Value {
    Value::Array(candidates.iter().map(Candidate::to_json).collect())
}
