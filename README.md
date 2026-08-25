# osswitch

Osswitch coordinates keyboard, pointer, clipboard, and a shared LG WebOS
display as one host-switch transaction. It combines a generalized Lan Mouse
fork with a Linux TV controller and native deployment automation.

## Components

- `lan-mouse/`: cross-platform input and clipboard transport. The default build
  retains the GTK application; no-GTK binaries are additional service assets.
- `tv-multiview/`: Linux controller that owns LG SSAP observation, HDMI input
  changes, two-phase switch grants, and fail-local recovery.
- `deploy/`: Ansible native-build and verified GitHub-release deployment modes
  for Linux, macOS, and Windows Lan Mouse hosts.
- `docs/` and `tla/`: architecture decisions, implementation plans, and formal
  safety/liveness models.

Lan Mouse talks to `tv-multiview` through its authenticated HTTP controller
client. Only `tv-multiview` talks LG SSAP to the TV. Input ownership remains on
the configured Lan Mouse server until the controller verifies the display and
remote-input readiness transition.

## Build And Test

The repository is one Cargo workspace with one lockfile. Rustup reads the root
`rust-toolchain.toml` and installs the pinned Rust toolchain and required
components:

```bash
cargo check --locked --workspace
cargo test --locked --workspace
cargo build --locked -p lan-mouse
cargo build --locked --profile lan-mouse-release -p lan-mouse
cargo build --locked --release -p tv-multiview
```

The first Lan Mouse build uses its default features, including GTK. Native
service builds disable default features and select only the platform-specific
input backends documented in `deploy/`.

## Deployment

Start with the sanitized examples in `deploy/`:

```bash
cd deploy
cp inventory.example.ini inventory.ini
cp group_vars/all.example.yml group_vars/all.yml
ansible-playbook -i inventory.ini playbook.yml
```

Real inventory, credentials, fingerprints, TV addresses, and display mappings
remain ignored local files. See `deploy/README.md` for native-build and immutable
release-manifest deployment semantics.

## Releases

One release tag identifies the workspace source and complete native artifact
set. Default GTK archives remain available alongside no-GTK Linux, Windows, and
macOS assets. The initial controller asset is Linux x86_64 only. Every archive
contains applicable license and README material, and
`osswitch-release-manifest.json` binds the release ID, tag commit, names, sizes,
and SHA-256 digests.

Release automation creates and verifies a draft before publication. It does not
deploy binaries or restart hosts.

## License And Attribution

The repository's original osswitch components are licensed under
GPL-3.0-or-later; see `LICENSE`. The imported Lan Mouse history, attribution,
and component license are retained under `lan-mouse/`. Third-party dependencies
remain governed by their own licenses.
