# avm-mcp-tools

JSON-Schema tool signatures for AVM: **generate**, **register**, **validate**.

An agent that is about to call a tool needs to know two things — *what can I
call?* and *is this call well-formed?* This crate answers both with plain
JSON-Schema (no bespoke schema language), and the gateway serves it over
`GET /tools/schema` and `POST /tools/validate`.

## Pieces

| Type | Role |
|---|---|
| `ToolSchema` | one tool signature: name, description, `input_schema`, optional `output_schema`, source, fingerprint |
| `ManagedToolSet` | registry of every callable tool, keyed by name, with compiled validators cached |
| `SchemaValidator` | a compiled `jsonschema` validator for one tool's input schema |
| `ToolSource` | provenance: which MCP server, over `builtin` / `stdio` / `http` |
| `Compatibility` | verdict when comparing two revisions of a schema |

## Functions

| Function | Purpose |
|---|---|
| `to_json_schema::<T>()` | generate a schema from a native Rust argument struct (`schemars`) |
| `from_mcp_definition(source, &json)` | parse one entry of an MCP `tools/list` response |
| `validate_call(&schema, &args)` | one-shot validation (compiles each call; use `ManagedToolSet` on hot paths) |
| `ManagedToolSet::ingest_listing(...)` | ingest a whole `tools/list` response from one server |
| `ManagedToolSet::assert_callable(...)` | the dispatch gate: `Ok(())` or the reason to refuse |

## Example

```rust
use avm_mcp_tools::{ManagedToolSet, ToolSource};
use serde_json::json;

let mut tools = ManagedToolSet::with_builtins();
tools.ingest_listing(
    ToolSource::stdio("filesystem", "npx -y @modelcontextprotocol/server-filesystem /srv"),
    &listing_from_mcp_server,
)?;

// Pre-dispatch gate
tools.assert_callable("read_file", &json!({ "path": "/srv/readme.md" }))?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Why generated schemas for built-ins

The gateway's local tools (`avm_memory_read`, `avm_memory_write`,
`avm_job_submit`, `avm_job_status`) derive `JsonSchema` on the very structs
their handlers deserialize. The published signature therefore cannot drift from
the implementation — change the struct and the schema (and its `fingerprint`)
changes with it.

See `IMPLEMENTATION_PLAN.md` § *Tool Schema & Validation* for the full design,
MCP scan flow, and versioning strategy.
