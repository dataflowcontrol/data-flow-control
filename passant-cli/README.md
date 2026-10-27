# Passant CLI catalog input

`rewrite`, `explain`, and `plan` accept `--catalog PATH`, a JSON
`CatalogSnapshot` supplied by the caller. Every policy SOURCE and SINK must have
an entry, and referenced policy columns must exist in that entry. The CLI does
not infer a schema from policy text or SQL.

Example `catalog.json`:

```json
{
  "dialect": "duckdb",
  "tables": {
    "events": {
      "columns": ["id", "approved"],
      "types": {"id": "INTEGER", "approved": "BOOLEAN"}
    },
    "reports": {"columns": ["id"], "types": {"id": "INTEGER"}}
  }
}
```

```sh
passant rewrite 'INSERT INTO reports SELECT id FROM events' \
  --catalog catalog.json \
  --policy 'SOURCE events SINK reports CONSTRAINT events.approved = TRUE ON FAIL REMOVE'
```

Use schema metadata for the actual tables against which the emitted SQL will
run. A missing or stale entry is not repaired by guessing. `parse-policy` only
parses policy syntax; it does not register or validate a policy against a database.
