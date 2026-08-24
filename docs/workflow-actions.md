# Workflow Dependency Review

This ledger records executable workflow dependencies reviewed for the root
GitHub Actions configuration. Workflow files use immutable commit IDs, not the
human-readable tags shown here.

| Dependency | Reviewed commit | Reference | Purpose |
|---|---|---|---|
| `actions/checkout` | `11d5960a326750d5838078e36cf38b85af677262` | `v4` | Read the exact event or release commit without persisting credentials. |
| `actions/upload-artifact` | `ea165f8d65b6e75b540449e92b4886f43607fa02` | `v4` | Transfer one native archive per build job to the protected draft job. |
| `actions/download-artifact` | `d3f86a106a0bac45b974a628896c90dbdf5c8093` | `v4` | Assemble the complete artifact matrix without resolving a release. |
| `actions/cache` | `0057852bfaa89a56745cba8c7296529d2fc39830` | `v4` | Cache the pinned Windows GTK build output. |
| `actions/setup-python` | `a26af69be951a213d495a4c3e4e4022e16d87065` | `v5` | Select the Python runtime used by pinned `gvsbuild`. |
| `rhysd/actionlint` | `03d0035246f3e81f36aed592ffb4bebf33a03106` | `v1.7.7` | Validate root workflow syntax from source through Go module verification. |

Release build tools are also version-bound: `cargo-bundle` 0.11.0 and
`gvsbuild` 2026.6.0. They are executable package dependencies rather than
GitHub Actions and remain subject to the lockfile and private-staging review.

The refs were resolved directly from their upstream Git repositories on
2026-08-23. Re-resolve and review every pin in private staging before public
visibility; do not update a commit merely because its mutable tag moved.
