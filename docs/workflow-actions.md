# Workflow Dependency Review

This ledger records executable workflow dependencies reviewed for the root
GitHub Actions configuration. Workflow files use immutable commit IDs, not the
human-readable tags shown here.

| Dependency | Reviewed commit | Reference | Purpose |
|---|---|---|---|
| `actions/checkout` | `11d5960a326750d5838078e36cf38b85af677262` | `v4` | Read the exact event or release commit without persisting credentials. |
| `rhysd/actionlint` | `03d0035246f3e81f36aed592ffb4bebf33a03106` | `v1.7.7` | Validate root workflow syntax from source through Go module verification. |

The refs were resolved directly from their upstream Git repositories on
2026-08-23. Re-resolve and review every pin in private staging before public
visibility; do not update a commit merely because its mutable tag moved.
