use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

use crate::model::*;
use crate::ports::{EvidenceStore, GuardEvaluator};

/// Raised when an `expr` guard references `$.context.X` (or another rooted
/// scope) for a path that does not resolve to *any* value — distinct from a
/// path that resolves to an explicit `null`. SPEC §9 mandates the runtime
/// fail fast on this case rather than silently coercing the missing read
/// to `null` (which would let the guard evaluate to a meaningless `false`).
#[derive(Debug, Error)]
#[error("GUARD_UNSET_SLOT: guard reads `{path}` which is not set on the workflow context")]
pub struct UnsetSlotError {
    pub path: String,
}

/// The built-in `GuardEvaluator`. Handles `permission`, `role`, `expr`,
/// and `evidence` kinds out of the box. The `evidence` kind needs an
/// `EvidenceStore` to check against — without one it always passes (handy
/// for tests).
pub struct DefaultGuardEvaluator {
    evidence: Option<Arc<dyn EvidenceStore>>,
}

impl Default for DefaultGuardEvaluator {
    fn default() -> Self {
        Self::new()
    }
}

impl DefaultGuardEvaluator {
    pub fn new() -> Self {
        Self { evidence: None }
    }

    pub fn with_evidence(evidence: Arc<dyn EvidenceStore>) -> Self {
        Self {
            evidence: Some(evidence),
        }
    }
}

#[async_trait]
impl GuardEvaluator for DefaultGuardEvaluator {
    async fn evaluate(
        &self,
        guard: &Value,
        instance: &WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
    ) -> anyhow::Result<bool> {
        match guard.get("kind").and_then(Value::as_str).unwrap_or("") {
            "permission" => {
                let required = guard
                    .get("permission")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Ok(principal.permissions.iter().any(|p| p == required))
            }

            "role" => {
                let required = guard
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Ok(principal.roles.iter().any(|r| r == required))
            }

            "expr" | "jsonpath" => {
                if guard.get("kind").and_then(Value::as_str) == Some("jsonpath") {
                    tracing::warn!("guard kind \"jsonpath\" is deprecated; use \"expr\" instead");
                }
                let expr = guard.get("expr").and_then(Value::as_str).unwrap_or("");
                eval_expr(expr, &instance.context, arguments, &instance.input)
            }

            "all_of" => {
                let inner = guard
                    .get("guards")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for g in inner {
                    let pass = self.evaluate(&g, instance, arguments, principal).await?;
                    if !pass {
                        return Ok(false);
                    }
                }
                Ok(true)
            }

            "any_of" => {
                let inner = guard
                    .get("guards")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if inner.is_empty() {
                    // Vacuously: nothing to satisfy → false (consistent with `requires` semantics).
                    return Ok(false);
                }
                // SPEC §9 interplay with `any_of`: a sub-guard reading an
                // unset slot is *not* a workflow-level failure if a sibling
                // clause passes — the author explicitly opted into "any of
                // these works". Errors are remembered and only surfaced if
                // no sibling satisfies. This preserves fail-fast for the
                // "no clause covered me" case without breaking the
                // declarative use of `any_of` over partially-written slots.
                let mut first_err: Option<anyhow::Error> = None;
                for g in inner {
                    match self.evaluate(&g, instance, arguments, principal).await {
                        Ok(true) => return Ok(true),
                        Ok(false) => {}
                        Err(e) => {
                            if first_err.is_none() {
                                first_err = Some(e);
                            }
                        }
                    }
                }
                match first_err {
                    Some(e) => Err(e),
                    None => Ok(false),
                }
            }

            "not" => {
                let inner = guard
                    .get("guard")
                    .ok_or_else(|| anyhow::anyhow!("`not` guard needs a `guard:` body"))?;
                let pass = self.evaluate(inner, instance, arguments, principal).await?;
                Ok(!pass)
            }

            "evidence" => {
                // `requires` accepts either a string (count >= 1) or
                // `{ kind: foo, count: N }` for quorums.
                let requirements: Vec<(String, usize)> = guard
                    .get("requires")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(parse_evidence_requirement)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                // No store wired? Permissive — useful for unit tests that
                // only care about non-evidence guards.
                let Some(store) = &self.evidence else {
                    return Ok(true);
                };

                let recorded = store.list(&instance.id).await?;
                Ok(requirements.iter().all(|(kind, count)| {
                    recorded.iter().filter(|e| &e.kind == kind).count() >= *count
                }))
            }

            _ => Ok(false),
        }
    }
}

fn parse_evidence_requirement(v: &Value) -> Option<(String, usize)> {
    if let Some(s) = v.as_str() {
        return Some((s.to_string(), 1));
    }
    if let Some(obj) = v.as_object() {
        let kind = obj.get("kind").and_then(Value::as_str)?.to_string();
        let count = obj
            .get("count")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(1);
        return Some((kind, count));
    }
    None
}

/// Evaluate a small expression of the form `<operand> <op> <operand>`.
///
/// Operands can be:
///   - A path: `$.context.x`, `$.arguments.y`, `$.workflow.input.z`
///   - A number: `42`, `3.14`
///   - A string: `"value"` or `'value'`
///   - A bool: `true`, `false`
///   - `null`
///
/// Operators:
///   - `==`, `!=`: any two same-typed operands (or null)
///   - `<`, `<=`, `>`, `>=`: numbers only
///
/// Path resolution semantics:
///
/// - `$.context.X` for an unset slot → **fail-fast** with [`UnsetSlotError`]
///   (SPEC §9: "a guard hitting an unset slot fails fast with rich context,
///   never a silent `false`"). A slot explicitly set to JSON `null` resolves
///   to `Value::Null` and is *not* an unset-slot error.
/// - `$.arguments.X` / `$.workflow.input.X` for a missing path resolve to
///   `Value::Null` (caller-controlled scopes; absent fields are legitimate).
///
/// `null == null` is true; `null` compared to anything else is false
/// (except `!=` which inverts).
fn eval_expr(
    expr: &str,
    context: &Value,
    arguments: &Value,
    input: &Value,
) -> anyhow::Result<bool> {
    let Some((left, op, right)) = parse_binary_expr(expr) else {
        return Ok(false);
    };

    let l = resolve_operand(left, context, arguments, input)?;
    let r = resolve_operand(right, context, arguments, input)?;

    Ok(compare_values(&l, op, &r))
}

fn resolve_operand(
    s: &str,
    context: &Value,
    arguments: &Value,
    input: &Value,
) -> anyhow::Result<Value> {
    let s = s.trim();

    // Quoted string literal.
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        return Ok(Value::String(s[1..s.len() - 1].to_string()));
    }
    // Bool / null.
    match s {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        "null" => return Ok(Value::Null),
        _ => {}
    }
    // Number.
    if let Ok(n) = s.parse::<f64>() {
        return Ok(serde_json::Number::from_f64(n)
            .map(Value::Number)
            .unwrap_or(Value::Null));
    }
    // Path — `$.context.*` fails fast on missing; other scopes coalesce.
    if let Some(path) = s.strip_prefix("$.context.") {
        return match context.pointer(&path_to_pointer(path)) {
            Some(v) => Ok(v.clone()),
            None => Err(UnsetSlotError {
                path: format!("$.context.{path}"),
            }
            .into()),
        };
    }
    if let Some(path) = s.strip_prefix("$.arguments.") {
        return Ok(arguments
            .pointer(&path_to_pointer(path))
            .cloned()
            .unwrap_or(Value::Null));
    }
    if let Some(path) = s.strip_prefix("$.workflow.input.") {
        return Ok(input
            .pointer(&path_to_pointer(path))
            .cloned()
            .unwrap_or(Value::Null));
    }
    Ok(Value::Null)
}

/// Convert a dot-notation path (e.g. `items[0].name` or `items.0.name`)
/// into a JSON Pointer string (e.g. `/items/0/name`).
pub(crate) fn path_to_pointer(path: &str) -> String {
    let mut result = String::with_capacity(path.len() + 1);
    result.push('/');
    let mut i = 0;
    let bytes = path.as_bytes();
    while i < bytes.len() {
        match bytes[i] {
            b'.' => result.push('/'),
            b'[' => {
                result.push('/');
                i += 1;
                while i < bytes.len() && bytes[i] != b']' {
                    result.push(bytes[i] as char);
                    i += 1;
                }
                // skip the closing ']'
            }
            c => result.push(c as char),
        }
        i += 1;
    }
    result
}

fn compare_values(a: &Value, op: &str, b: &Value) -> bool {
    // Numeric comparisons work whenever both sides are numbers.
    if let (Some(an), Some(bn)) = (a.as_f64(), b.as_f64()) {
        return match op {
            "==" => (an - bn).abs() < f64::EPSILON,
            "!=" => (an - bn).abs() >= f64::EPSILON,
            "<" => an < bn,
            "<=" => an <= bn,
            ">" => an > bn,
            ">=" => an >= bn,
            _ => false,
        };
    }
    // String operations
    match op {
        "starts_with" => {
            let sa = a.as_str().unwrap_or("");
            let sb = b.as_str().unwrap_or("");
            sa.starts_with(sb)
        }
        "contains" => {
            let sa = a.as_str().unwrap_or("");
            let sb = b.as_str().unwrap_or("");
            sa.contains(sb)
        }
        // Strings, bools, nulls — equality only.
        "==" => a == b,
        "!=" => a != b,
        _ => false,
    }
}

fn parse_binary_expr(expr: &str) -> Option<(&str, &str, &str)> {
    // Order matters: "<=" / ">=" / "==" / "!=" must be tried before "<" / ">".
    // We also need to skip operators that fall inside quoted strings — a
    // string literal "is not" mustn't cause the splitter to break on `<`.
    for op in ["starts_with", "contains", "<=", ">=", "==", "!=", "<", ">"] {
        if let Some(idx) = find_op_outside_quotes(expr, op) {
            let (left, rest) = expr.split_at(idx);
            let right = &rest[op.len()..];
            return Some((left.trim(), op, right.trim()));
        }
    }
    None
}

/// Find the byte index of `needle` in `haystack` skipping any occurrence
/// inside single- or double-quoted regions.
fn find_op_outside_quotes(haystack: &str, needle: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i + needle_bytes.len() <= bytes.len() {
        let c = bytes[i];
        if !in_single && c == b'"' {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_double && c == b'\'' {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single && !in_double && bytes[i..i + needle_bytes.len()] == *needle_bytes {
            return Some(i);
        }
        i += 1;
    }
    None
}
