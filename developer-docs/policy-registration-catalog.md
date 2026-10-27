# Catalog-backed policy registration

Policy registration requires an explicit catalog entry for
**every declared SOURCE and SINK**, even when the constraint does not reference a
column from that table. Referenced physical columns are validated against those
entries. Resolution kind does not exempt a policy from validation.

The Python `Connection` obtains these entries from the adapter's actual database
metadata before registration. A standalone `Planner` must receive an explicit
catalog snapshot first. A native caller supplies a `TableCatalog` or catalog
snapshot. Do not construct fictional tables/columns by collecting policy
references: that would make misspellings validate themselves.

Native API changes:

- `PassantRewriter::register_policy` returns `Result<(), RewriteError>`.
- `register_validated_policy` is a compatibility alias for the same validated path.
- `register_policies` validates the entire batch before allocating policy IDs.
- `from_policies` is fallible; physical policies need
  `from_policies_with_catalog(policies, catalog)` instead.
- `PolicyStore::register` and `register_policies` require a catalog and aggregate
  registry and return `Result`. Unvalidated insertion is not public.
- Internal branch views retain the parent policy identity and known catalog;
  compiler-generated virtual relations are not separately registered as physical
  user tables.
- `PassantPlanner::plan_query_with_rewriter` and registered explain use the
  supplied full rewriter context. A catalog-free planning call can report a
  registration error; it cannot silently install unvalidated physical policies.

Rejected single or batch registration leaves existing policies and the next
stable policy ID unchanged. There is no policy-replacement API: deletion and
subsequent registration remain distinct operations. This change does not alter
backend catalog-snapshot replacement semantics.

Unqualified policy columns remain rejected by the existing syntax contract.
Source/sink aliases, schema-qualified names and quoted column names are checked
against their declared physical table entries.

## DIMENSION dependency binding

DIMENSION SELECT subqueries now bind physical input tables and every referenced
column against explicit catalog entries before their projected output names are
accepted. This validates name binding and catalog columns, not complete SQL type
checking or backend function availability. Supported scopes include physical-table aliases, schema-qualified
names, unambiguous unqualified input columns, ON joins, filter/aggregate
expressions and ORDER BY output aliases. Unknown or ambiguous inputs fail the
entire registration batch without allocating policy IDs.

The current conservative binder explicitly rejects CTEs, derived/lateral tables,
expression/correlated subqueries, table functions, temporal tables, table-column
alias lists, NATURAL/USING joins, set operations, wildcard projections and unnamed
computed outputs. Output-alias references outside ORDER BY are not resolved
unless also valid input-column references. These are current implementation
limits, not a claim such SQL cannot be validated. No database query is executed
to infer an output schema or validate dependencies.

## No-SINK output applicability

Policies may reference `_OUTPUT_` without SINK. At rewrite time, the
whole policy applies only when the final public output schema contains **every**
referenced output attribute. A missing attribute skips that policy, not just an
atom of its constraint. A present NULL remains a value subject to the policy.
Standalone SELECT uses public aliases/catalog-expanded stars; INSERT uses target
column names, not private SELECT aliases. Ambiguous duplicate names are errors,
not absent attributes. Internal provenance columns do not count as public output.

Unknown schema is not an empty schema: unproven wildcard bindings, dialect-renamed
duplicate derived outputs, BY NAME set operations and unsupported schema forms
fail explicitly rather than silently skipping applicable policies. Explicit root
projections can prove absence without inspecting unused inner schemas. Active
output policies resolve at the final endpoint, never against an inner CTE's
accidental aliases. Physical SOURCE/SINK validation remains unconditional.
DDL through `Connection` invalidates and refreshes actual catalog metadata before
subsequent rewriting; no candidate SQL is executed for this metadata refresh.

Non-distributive PartialPush source aggregation with an active no-SINK `_OUTPUT_`
reference is not implemented. It rejects before SQL execution with the policy ID
and constraint; a proven missing output still skips the policy normally. Existing
INSERT-output aggregate plans with explicit final-endpoint handling remain usable.
Successful INSERT retains its INSERT wrapper; output checking does not turn a write
into a SELECT. If schema changes occur through the raw database connection rather
than `Connection`, call `refresh_catalog(force=True)` before rewriting.
