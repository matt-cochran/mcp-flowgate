//! Argument structs + schema helpers for the MCP tool surface.
//!
//! One `*Args` struct per tool. Both the published JSON Schema (via
//! `schemars::JsonSchema`) and the per-handler argument extraction (via
//! `serde::Deserialize`) come from these definitions.
//!
//! Required-field policy is encoded twice on purpose: the per-call required
//! list passed to `schema_for_args` controls what the published schema
//! advertises; the handler's `.ok_or_else(... "is required")` controls what
//! the runtime rejects. They're maintained as a pair because the published
//! surface and the runtime have diverged historically (some schema-required
//! fields are silently defaulted by the runtime), and the parity tests fix
//! that contract in place. Every field is `Option<T>` so the deserializer
//! never produces serde's default missing-field error — handlers raise the
//! canonical "<field> is required" message instead.
//!
//! Tool-specific schema shims (`integer_schema`, `object_schema`,
//! `discovery_kind_schema`) override the default schemars output so the
//! published schema matches what callers see today.

use std::sync::Arc;

use rmcp::model::JsonObject;
use schemars::gen::{SchemaGenerator, SchemaSettings};
use schemars::schema::{InstanceType, Schema, SchemaObject};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchArgs {
    pub query: Option<String>,
    #[schemars(schema_with = "discovery_kind_schema")]
    pub kind: Option<String>,
    #[serde(default = "default_limit")]
    #[schemars(schema_with = "limit_schema")]
    pub limit: u64,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DescribeArgs {
    pub id: Option<String>,
    /// SPEC §8.2 — when present, resolve guidance bodies from this
    /// workflow's pinned snapshot so an in-flight instance sees the
    /// body that existed at `workflow.start`, not whatever the live
    /// config currently says. Workflow / capability lookups ignore it.
    pub workflow_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StartArgs {
    pub definition_id: Option<String>,
    #[schemars(schema_with = "object_schema")]
    pub input: Option<Value>,
    /// SPEC §20.2 — optional trace id propagated to every audit event
    /// for the created workflow instance. Opaque to the gateway.
    pub trace_id: Option<String>,
    /// SPEC §20.2 — optional run id for grouping related workflow
    /// instances. Opaque to the gateway.
    pub run_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GetArgs {
    pub workflow_id: Option<String>,
    /// SPEC §20.2 — optional per-call trace id override. The instance's
    /// persisted `trace_id` is used by default.
    pub trace_id: Option<String>,
    pub run_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubmitArgs {
    pub workflow_id: Option<String>,
    #[schemars(schema_with = "integer_schema")]
    pub expected_version: Option<u64>,
    pub transition: Option<String>,
    #[schemars(schema_with = "object_schema")]
    pub arguments: Option<Value>,
    /// SPEC §6.3 — optional model-authored summary. Stored to
    /// `context.summary` on commit; surfaced in every response.
    pub summary: Option<String>,
    /// SPEC §20.2 — optional per-submit trace id override.
    pub trace_id: Option<String>,
    pub run_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExplainArgs {
    pub workflow_id: Option<String>,
    pub transition: Option<String>,
}

pub(crate) fn default_limit() -> u64 {
    10
}

// ---------- per-field schema overrides ----------------------------------
//
// Schemars's default schemas for `u64`/`Option<Value>` carry extra hints
// (`format: uint64`, `minimum: 0`, `additionalProperties: true`) that the
// previous hand-written schemas didn't. These shims keep the published
// schema byte-equivalent to the pre-refactor surface.

pub(crate) fn integer_schema(_: &mut SchemaGenerator) -> Schema {
    SchemaObject {
        instance_type: Some(InstanceType::Integer.into()),
        ..Default::default()
    }
    .into()
}

pub(crate) fn limit_schema(gen: &mut SchemaGenerator) -> Schema {
    let mut schema = match integer_schema(gen) {
        Schema::Object(o) => o,
        Schema::Bool(_) => unreachable!("integer_schema always returns Schema::Object"),
    };
    schema.metadata().default = Some(json!(default_limit()));
    schema.into()
}

pub(crate) fn object_schema(_: &mut SchemaGenerator) -> Schema {
    SchemaObject {
        instance_type: Some(InstanceType::Object.into()),
        ..Default::default()
    }
    .into()
}

pub(crate) fn discovery_kind_schema(_: &mut SchemaGenerator) -> Schema {
    SchemaObject {
        instance_type: Some(InstanceType::String.into()),
        enum_values: Some(vec![
            json!("workflow"),
            json!("capability"),
            json!("connection"),
        ]),
        ..Default::default()
    }
    .into()
}

/// Build the rmcp `Tool.input_schema` for a typed `*Args` struct. The
/// `required` list is supplied explicitly because some schema-required
/// fields are silently defaulted by the runtime — see the args-struct
/// comment block above.
pub(crate) fn schema_for_args<T: JsonSchema>(required: &[&'static str]) -> Arc<JsonObject> {
    let generator = SchemaSettings::draft07()
        .with(|s| {
            s.option_add_null_type = false;
            s.inline_subschemas = true;
            s.meta_schema = None;
        })
        .into_generator();
    let root = generator.into_root_schema_for::<T>();
    let mut value =
        serde_json::to_value(&root).expect("schemars produces JSON-serializable schema");
    let obj = value
        .as_object_mut()
        .expect("root schema is always an object");
    obj.remove("$schema");
    obj.remove("title");
    obj.remove("definitions");
    obj.remove("description");

    if let Some(Value::Object(props)) = obj.get_mut("properties") {
        for (_, v) in props.iter_mut() {
            if let Value::Object(field) = v {
                // Strip schemars hints the legacy hand-written schemas
                // didn't carry: numeric `format`/`minimum`, the recursive
                // `additionalProperties: true` schemars stamps on
                // `Map<String, Value>`, and field doc-comments.
                field.remove("format");
                field.remove("minimum");
                field.remove("additionalProperties");
                field.remove("description");
            }
        }
    }

    if required.is_empty() {
        obj.remove("required");
    } else {
        obj.insert("required".into(), json!(required));
    }
    obj.insert("additionalProperties".into(), Value::Bool(false));
    Arc::new(value.as_object().cloned().expect("still an object"))
}

/// Hand-built schema for `gateway.home`, which takes no arguments. Going
/// through schemars for a struct with zero fields works but emits an empty
/// `properties` map and no `required` key — same result, but a one-liner
/// here is cleaner than spelling out a `struct HomeArgs;` derive just to
/// produce `{}`.
pub(crate) fn empty_object_schema() -> Arc<JsonObject> {
    let mut obj = serde_json::Map::new();
    obj.insert("type".into(), Value::String("object".into()));
    obj.insert("properties".into(), Value::Object(serde_json::Map::new()));
    obj.insert("additionalProperties".into(), Value::Bool(false));
    Arc::new(obj)
}
