//! Capability discovery and search.
//!
//! Discovery is a separate HATEOAS layer from the workflow runtime: a model
//! starts at `gateway.home`, calls `gateway.search` to find a relevant
//! workflow or proxy capability, follows the returned link to start it, and
//! from there is in workflow-HATEOAS land.
//!
//! The MVP uses an in-memory lexical scorer over a flat `Vec<DiscoveryItem>`.
//! The trait is async so backends like Tantivy or vector indexes can plug in
//! later without changing callers.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryKind {
    Workflow,
    Capability,
    Connection,
    /// A reusable guidance fragment ("skill"). The lookup id is the fragment's
    /// `subject`; `gateway.describe(subject)` returns its `verb` + `body`.
    Guidance,
}

impl DiscoveryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DiscoveryKind::Workflow => "workflow",
            DiscoveryKind::Capability => "capability",
            DiscoveryKind::Connection => "connection",
            DiscoveryKind::Guidance => "guidance",
        }
    }
}

/// SPEC §5.4.1 — the eight closed cognitive-operation verbs that may tag a
/// guidance fragment. This is a closed enum on purpose: no `Other(String)`
/// escape variant, no `#[serde(other)]`. Authoring a new verb requires a
/// deliberate spec amendment, not a config-time string. Unknown verbs fail
/// config-load with `INVALID_VERB` (see [`Verb::from_token`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    /// Classify, prioritize, route.
    Triage,
    /// Find root cause.
    Diagnose,
    /// Design approach before acting.
    Plan,
    /// Produce / generate the artifact.
    Implement,
    /// Evaluate against criteria.
    Review,
    /// Restructure preserving behavior.
    Refactor,
    /// Build understanding (self-explain or teach others).
    Explain,
    /// Assemble parts into a whole.
    Compose,
}

impl Verb {
    /// The closed set of allowed verb tokens, in spec order. Returned as
    /// `&'static [&'static str]` so error messages can list them verbatim
    /// without per-call allocation.
    pub const ALL_TOKENS: &'static [&'static str] = &[
        "triage", "diagnose", "plan", "implement", "review", "refactor", "explain", "compose",
    ];

    pub fn as_token(self) -> &'static str {
        match self {
            Verb::Triage => "triage",
            Verb::Diagnose => "diagnose",
            Verb::Plan => "plan",
            Verb::Implement => "implement",
            Verb::Review => "review",
            Verb::Refactor => "refactor",
            Verb::Explain => "explain",
            Verb::Compose => "compose",
        }
    }

    /// Parse a verb token, case-sensitively. Returns `None` for any string not
    /// in the closed set. Whitespace, uppercase, hyphen, dot, or any other
    /// deviation rejects.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "triage" => Some(Verb::Triage),
            "diagnose" => Some(Verb::Diagnose),
            "plan" => Some(Verb::Plan),
            "implement" => Some(Verb::Implement),
            "review" => Some(Verb::Review),
            "refactor" => Some(Verb::Refactor),
            "explain" => Some(Verb::Explain),
            "compose" => Some(Verb::Compose),
            _ => None,
        }
    }
}

/// SPEC §5.3 — required lifecycle marker on every guidance fragment. Closed
/// enum; no silent default. A fragment without `lifecycle` fails config-load
/// with `MISSING_LIFECYCLE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    Experimental,
    Stable,
    Deprecated,
}

impl Lifecycle {
    pub const ALL_TOKENS: &'static [&'static str] = &["experimental", "stable", "deprecated"];

    pub fn as_token(self) -> &'static str {
        match self {
            Lifecycle::Experimental => "experimental",
            Lifecycle::Stable => "stable",
            Lifecycle::Deprecated => "deprecated",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "experimental" => Some(Lifecycle::Experimental),
            "stable" => Some(Lifecycle::Stable),
            "deprecated" => Some(Lifecycle::Deprecated),
            _ => None,
        }
    }
}

/// SPEC §5.4.2 — the blessed top-level segments for guidance fragment
/// subjects. A subject's first dotted segment MUST be one of these, or the
/// config produces an `INVALID_SUBJECT_ROOT` diagnostic (error under
/// `strict_namespacing: true`, warning otherwise).
///
/// The list combines:
/// - Six domain-themed roots (`authoring`, `debug`, `deploy`, `import`,
///   `lifecycle`, `review`) that group guidance by topic regardless of
///   which verb is appropriate.
/// - Eight verb-mirror roots — one per closed verb in [`Verb::ALL_TOKENS`] —
///   so authors can group guidance by the cognitive operation it primes
///   (e.g. `implement.edit.constrained`, `diagnose.codebase.search`).
///
/// Two roots (`plan` and `review`) appear in BOTH categories; they are
/// listed once. Total: 12 blessed roots.
pub const BLESSED_SUBJECT_ROOTS: &[&str] = &[
    // Domain-themed (groups guidance by topic).
    "authoring",
    "debug",
    "deploy",
    "import",
    "lifecycle",
    // Verb-mirror (groups guidance by cognitive operation).
    "triage",
    "diagnose",
    "plan",       // also a verb
    "implement",
    "review",     // also a verb
    "refactor",
    "explain",
    "compose",
];

/// A single thing that can be discovered: a workflow, a proxy capability, or
/// a configured connection. Everything carries enough metadata to score it
/// against a query and to render a HATEOAS link template that lets the caller
/// act on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryItem {
    pub id: String,
    pub kind: DiscoveryKind,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub examples: Vec<String>,
    /// Author-provided synonyms. Indexed with the same weight as tags so a
    /// capability named `release.promote` can declare `aliases: ["deploy", "ship"]`
    /// and be found by those terms.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Free-form text that lexical scoring can search over. Index-builders
    /// fill this with concatenated state names, transition names, etc.
    #[serde(default)]
    pub text: String,
    /// HATEOAS templates for what to do with this item.
    #[serde(default)]
    pub links: Vec<DiscoveryLink>,
    /// Guidance fragments only: the fragment's space-free `verb` (`apply`,
    /// `check`, ...). `None` for non-guidance items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verb: Option<String>,
    /// Guidance fragments only: the fragment's static markdown body returned
    /// by `gateway.describe`. `None` for non-guidance items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// SPEC §5.3 — fragment provenance. Examples: `config` (declared inline
    /// in workflow YAML), `git+https://github.com/org/repo@sha`. Used by the
    /// `gateway.skills.search` `source` filter (§17.6). `None` for
    /// non-guidance items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// A pre-built HATEOAS link attached to a `DiscoveryItem`. These are
/// "next-step" pointers — typically `workflow.start` for a workflow or a
/// `workflow.start` against `proxy_default` for a capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryLink {
    pub rel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// MCP tool name to call (`workflow.start`, `gateway.search`, ...).
    pub method: String,
    /// Pre-filled arguments for that tool call.
    pub args: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub score: f32,
    pub item: DiscoveryItem,
}

#[derive(Debug, Clone, Default)]
pub struct SearchRequest {
    pub query: String,
    pub kind: Option<DiscoveryKind>,
    pub limit: usize,
}

#[async_trait]
pub trait DiscoveryIndex: Send + Sync {
    async fn search(&self, request: SearchRequest) -> anyhow::Result<Vec<SearchHit>>;
    async fn describe(&self, id: &str) -> anyhow::Result<Option<DiscoveryItem>>;
    async fn list(&self, kind: Option<DiscoveryKind>) -> anyhow::Result<Vec<DiscoveryItem>>;
    async fn home(&self) -> anyhow::Result<Value> {
        Ok(default_home())
    }
}

fn default_home() -> Value {
    json!({
        "resource": { "type": "gateway", "id": "home" },
        "result": {
            "status": "ready",
            "message": "Available workflows and proxy capabilities can be discovered here."
        },
        "links": [
            {
                "rel": "search",
                "title": "Search workflows and capabilities",
                "method": "gateway.search",
                "args": { "query": "" },
                "inputSchema": {
                    "type": "object",
                    "required": ["query"],
                    "properties": {
                        "query": { "type": "string" },
                        "kind": { "type": "string", "enum": ["workflow", "capability", "connection"] },
                        "limit": { "type": "integer", "default": 10 }
                    },
                    "additionalProperties": false
                }
            },
            {
                "rel": "list_workflows",
                "title": "List configured workflows",
                "method": "gateway.search",
                "args": { "query": "", "kind": "workflow" }
            },
            {
                "rel": "list_capabilities",
                "title": "List proxy capabilities",
                "method": "gateway.search",
                "args": { "query": "", "kind": "capability" }
            }
        ]
    })
}

/// In-memory lexical discovery index. Construct via
/// `InMemoryDiscoveryIndex::from_config(config)` to populate from the parsed
/// gateway YAML, or via `new(items)` if you're building documents yourself.
#[derive(Default, Clone)]
pub struct InMemoryDiscoveryIndex {
    docs: Arc<Vec<DiscoveryItem>>,
}

impl InMemoryDiscoveryIndex {
    pub fn new(items: Vec<DiscoveryItem>) -> Self {
        Self {
            docs: Arc::new(items),
        }
    }

    pub fn from_config(config: &Value) -> Self {
        Self::new(crate::discovery_indexer::index_from_config(config))
    }

    pub fn extend(&mut self, items: impl IntoIterator<Item = DiscoveryItem>) {
        let mut owned =
            Arc::try_unwrap(std::mem::take(&mut self.docs)).unwrap_or_else(|arc| (*arc).clone());
        owned.extend(items);
        self.docs = Arc::new(owned);
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }
}

#[async_trait]
impl DiscoveryIndex for InMemoryDiscoveryIndex {
    async fn search(&self, request: SearchRequest) -> anyhow::Result<Vec<SearchHit>> {
        let limit = if request.limit == 0 {
            10
        } else {
            request.limit
        };
        let terms = tokenize(&request.query);
        let want_all = terms.is_empty();

        // Guidance fragments are looked up by known subject via
        // `gateway.describe` — they're not the answer to "what can I do?".
        // They stay in the index (so describe can find them) but are
        // excluded from search unless the caller asks for them explicitly
        // via `kind=guidance`.
        let mut hits: Vec<SearchHit> = self
            .docs
            .iter()
            .filter(|d| match request.kind {
                Some(k) => k == d.kind,
                None => d.kind != DiscoveryKind::Guidance,
            })
            .filter_map(|d| {
                let score = score_doc(d, &terms);
                if want_all || score > 0.0 {
                    Some(SearchHit {
                        score: if want_all { 1.0 } else { score },
                        item: d.clone(),
                    })
                } else {
                    None
                }
            })
            .collect();

        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.item.id.cmp(&b.item.id))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    async fn describe(&self, id: &str) -> anyhow::Result<Option<DiscoveryItem>> {
        Ok(self.docs.iter().find(|d| d.id == id).cloned())
    }

    async fn list(&self, kind: Option<DiscoveryKind>) -> anyhow::Result<Vec<DiscoveryItem>> {
        Ok(self
            .docs
            .iter()
            .filter(|d| match kind {
                Some(k) => k == d.kind,
                None => d.kind != DiscoveryKind::Guidance,
            })
            .cloned()
            .collect())
    }
}

// ---------- scoring ---------------------------------------------------------

fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

fn score_doc(doc: &DiscoveryItem, terms: &[String]) -> f32 {
    let title = doc.title.to_lowercase();
    let id = doc.id.to_lowercase();
    let desc = doc.description.to_lowercase();
    let text = doc.text.to_lowercase();
    let tags = doc.tags.join(" ").to_lowercase();
    let aliases = doc.aliases.join(" ").to_lowercase();

    terms.iter().fold(0.0_f32, |acc, term| {
        acc + field_score(&title, term, 6.0)
            + field_score(&id, term, 5.0)
            + field_score(&tags, term, 3.0)
            + field_score(&aliases, term, 3.0)
            + field_score(&desc, term, 2.0)
            + field_score(&text, term, 1.0)
    })
}

fn field_score(field: &str, term: &str, weight: f32) -> f32 {
    if field.contains(term) {
        return weight;
    }

    let words: Vec<&str> = field
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();

    if term.len() >= 2 && words.iter().any(|w| w.starts_with(term)) {
        return weight * 0.7;
    }

    if term.len() >= 4 {
        let best = words
            .iter()
            .map(|w| trigram_similarity(term, w))
            .fold(0.0_f32, f32::max);
        if best > 0.3 {
            return weight * best * 0.5;
        }
    }

    0.0
}

fn trigram_similarity(a: &str, b: &str) -> f32 {
    let ta = trigrams(a);
    let tb = trigrams(b);
    if ta.is_empty() && tb.is_empty() {
        return 1.0;
    }
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let intersection = ta.iter().filter(|t| tb.contains(t)).count();
    let union = ta.len() + tb.len() - intersection;
    if union == 0 {
        0.0
    } else {
        intersection as f32 / union as f32
    }
}

fn trigrams(s: &str) -> Vec<[u8; 3]> {
    let bytes = s.as_bytes();
    if bytes.len() < 3 {
        return vec![];
    }
    let mut out = Vec::with_capacity(bytes.len() - 2);
    for i in 0..bytes.len() - 2 {
        out.push([bytes[i], bytes[i + 1], bytes[i + 2]]);
    }
    out.sort();
    out.dedup();
    out
}
