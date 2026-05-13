use anyhow::anyhow;
use serde_json::{json, Value};

/// Apply an output-mapping object to the workflow's context.
///
/// Each mapping value is either:
/// - A **path string** like `"$.output.plan"` resolved against
///   `executor_output`, or any of the broader scopes (`$.context.*`,
///   `$.arguments.*`, `$.workflow.input.*`).
/// - An **operator object** for declarative computation: `{ add: [a, b] }`,
///   `{ subtract: […] }`, `{ multiply: […] }`, `{ divide: […] }`,
///   `{ set: <literal> }`. Operands may themselves be path strings or literal
///   numbers.
/// - Any other JSON literal — used as the value verbatim.
///
/// Numeric operations treat missing/null operands as 0 so a counter can be
/// incremented even before it's first written.
pub fn merge_output(
    context: &mut Value,
    mapping: Option<&Value>,
    arguments: &Value,
    workflow_input: &Value,
    executor_output: &Value,
) -> anyhow::Result<()> {
    let Some(mapping) = mapping.and_then(Value::as_object) else {
        return Ok(());
    };

    if !context.is_object() {
        return Err(anyhow!("workflow context must be an object"));
    }

    // Collect first so we can read context while building. The borrow checker
    // doesn't love &mut context + &context simultaneously.
    let pending: Vec<(String, Value)> = mapping
        .iter()
        .map(|(k, spec)| {
            let v = resolve_value(spec, arguments, context, workflow_input, executor_output);
            (k.clone(), v)
        })
        .collect();

    let obj = context.as_object_mut().unwrap();
    for (k, v) in pending {
        obj.insert(k, v);
    }
    Ok(())
}

/// Resolve a single mapping value against the available scopes.
///
/// Public so other parts of the runtime (link prefill, executor maps) can
/// reuse the same expression syntax — string paths, operator objects
/// (`{ add: [a, b] }`, `{ set: x }`, etc.), or literal pass-through.
pub fn resolve_value(
    spec: &Value,
    arguments: &Value,
    context: &Value,
    workflow_input: &Value,
    executor_output: &Value,
) -> Value {
    match spec {
        Value::String(s) => {
            // Strings starting with "$." are path expressions; everything
            // else is a literal. Lets authors write `base: "main"` instead
            // of having to wrap every literal in `{ set: "main" }`.
            if s.starts_with("$.") || s == "$" {
                read_in_scopes(s, arguments, context, workflow_input, Some(executor_output))
                    .unwrap_or(Value::Null)
            } else {
                Value::String(s.clone())
            }
        }

        Value::Object(obj) if obj.len() == 1 => {
            let (op, args) = obj.iter().next().unwrap();
            match op.as_str() {
                "set" => args.clone(),

                "add" | "subtract" | "multiply" | "divide" => {
                    let nums = match resolve_operands(
                        args,
                        arguments,
                        context,
                        workflow_input,
                        executor_output,
                    ) {
                        Some(n) => n,
                        None => return Value::Null,
                    };
                    if nums.len() != 2 {
                        return Value::Null;
                    }
                    let (a, b) = (nums[0], nums[1]);
                    let result = match op.as_str() {
                        "add" => a + b,
                        "subtract" => a - b,
                        "multiply" => a * b,
                        "divide" => {
                            if b == 0.0 {
                                return Value::Null;
                            }
                            a / b
                        }
                        _ => unreachable!(),
                    };
                    json_number(result)
                }

                "concat" => {
                    let parts = match args.as_array() {
                        Some(arr) => arr,
                        None => return Value::Null,
                    };
                    let mut result = String::new();
                    for part in parts {
                        let resolved = resolve_value(
                            part,
                            arguments,
                            context,
                            workflow_input,
                            executor_output,
                        );
                        match resolved {
                            Value::String(s) => result.push_str(&s),
                            Value::Number(n) => result.push_str(&n.to_string()),
                            Value::Bool(b) => result.push_str(&b.to_string()),
                            Value::Null => result.push_str("null"),
                            other => {
                                result.push_str(&serde_json::to_string(&other).unwrap_or_default())
                            }
                        }
                    }
                    Value::String(result)
                }

                _ => spec.clone(),
            }
        }

        other => other.clone(),
    }
}

/// Parse the operands of an arithmetic operator. Each operand is either a
/// path string or a literal number; missing/null path resolutions become 0.
fn resolve_operands(
    spec: &Value,
    arguments: &Value,
    context: &Value,
    workflow_input: &Value,
    executor_output: &Value,
) -> Option<Vec<f64>> {
    let arr = spec.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let resolved = match v {
            Value::String(s) => {
                read_in_scopes(s, arguments, context, workflow_input, Some(executor_output))
                    .unwrap_or(Value::Null)
            }
            other => other.clone(),
        };
        let n = match &resolved {
            Value::Null => 0.0,
            Value::Number(n) => n.as_f64().unwrap_or(0.0),
            _ => return None,
        };
        out.push(n);
    }
    Some(out)
}

fn json_number(n: f64) -> Value {
    if n.is_finite() {
        // Prefer integers when round.
        if n.fract() == 0.0 && n.abs() <= i64::MAX as f64 {
            return json!(n as i64);
        }
        json!(n)
    } else {
        Value::Null
    }
}

/// Reads `$.output[.path]` against the given executor output value, or returns
/// the executor output itself for `$` and `$.output`.
pub fn read_expr(value: &Value, expr: &str) -> Option<Value> {
    match expr {
        "$" | "$.output" => Some(value.clone()),
        _ => {
            let path = expr.strip_prefix("$.output.")?;
            value
                .pointer(&format!("/{}", path.replace('.', "/")))
                .cloned()
        }
    }
}

/// Reads any of the supported expression roots against the relevant scopes.
/// Used by the CLI executor and similar places that need late-bound values.
pub fn read_in_scopes(
    expr: &str,
    arguments: &Value,
    context: &Value,
    workflow_input: &Value,
    executor_output: Option<&Value>,
) -> Option<Value> {
    if let Some(path) = expr.strip_prefix("$.arguments.") {
        return arguments
            .pointer(&format!("/{}", path.replace('.', "/")))
            .cloned();
    }
    if let Some(path) = expr.strip_prefix("$.context.") {
        return context
            .pointer(&format!("/{}", path.replace('.', "/")))
            .cloned();
    }
    if let Some(path) = expr.strip_prefix("$.workflow.input.") {
        return workflow_input
            .pointer(&format!("/{}", path.replace('.', "/")))
            .cloned();
    }
    if let Some(out) = executor_output {
        if expr == "$.output" || expr == "$" {
            return Some(out.clone());
        }
        if let Some(path) = expr.strip_prefix("$.output.") {
            return out
                .pointer(&format!("/{}", path.replace('.', "/")))
                .cloned();
        }
    }
    None
}
