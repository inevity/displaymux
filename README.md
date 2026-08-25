# osswitch

Osswitch coordinates keyboard, pointer, clipboard, and a shared LG WebOS
display as one host-switch transaction. It combines a generalized Lan Mouse
fork with a Linux TV controller and native deployment automation.

## Capabilities

- Share keyboard and pointer input across Linux, macOS, and Windows hosts.
- Transfer clipboard text through an authenticated peer transport.
- Coordinate LG WebOS input selection with remote-input readiness.
- Keep keyboard and pointer ownership local until the requested switch is
  verified and committed.
- Recover to the configured server when a request expires, a peer disconnects,
  or display verification fails.
- Deploy native builds or digest-verified GitHub Release artifacts with the same
  Ansible playbook.

## How It Works

```text
Lan Mouse edge intent
        |
        v
authenticated controller request
        |
        v
tv-multiview observes LG WebOS + peer readiness
        |
        v
verified grant and atomic input commit
        |
        v
remote host receives keyboard, pointer, and clipboard ownership
```

The display route and input owner are separate state. A user may select a TV
input manually while the server retains input ownership; Osswitch only performs
automatic fallback while resolving an active switch transaction.

## Repository Components

- [`lan-mouse/`](lan-mouse/README.md): cross-platform input and clipboard
  transport. The default build retains the GTK application; no-GTK binaries
  are additional service assets.
- [`tv-multiview/`](tv-multiview/README.md): controller daemon that owns LG
  SSAP observation, HDMI input changes, two-phase switch grants, and fail-local
  recovery.
- [`deploy/`](deploy/README.md): Ansible native-build and verified
  GitHub-release deployment modes for Linux, macOS, and Windows hosts.
- [`docs/`](docs/) and [`tla/`](tla/README.md): architecture decisions,
  implementation plans, and formal safety/liveness models.

Lan Mouse talks to `tv-multiview` through its authenticated HTTP controller
client. Only `tv-multiview` talks LG SSAP to the TV. Input ownership remains on
the configured Lan Mouse server until the controller verifies the display and
remote-input readiness transition.

## Platform Support

| Component | Linux | macOS | Windows |
|---|---:|---:|---:|
| Lan Mouse GTK application | Yes | Yes | Yes |
| Lan Mouse no-GTK service | Yes | Yes | Yes |
| Clipboard transport | Yes | Yes | Yes |
| Packaged `tv-multiview` service | Yes | Not yet | Not yet |
| Native Ansible deployment | Yes | Yes | Yes |

The architecture colocates `tv-multiview` with the configured Lan Mouse
hub/server host; controller ownership is not intrinsically tied to Linux. The
current deployment selects Linux as that server and currently packages the
controller service only for Linux. macOS and Windows controller service/release
integration remains separate from the architecture.

## Build and Test

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
input backends documented under [`deploy/`](deploy/README.md). Platform-specific
system libraries required by the GTK and input backends are documented in the
Lan Mouse component repository history and deployment tasks.

For a smaller controller-only check:

```bash
cargo check --locked -p tv-multiview
cargo test --locked -p tv-multiview
```

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

The playbook supports two installation modes:

- `native_build` builds every native host from one Git commit, root lockfile,
  and vendor archive.
- `github_release` resolves one immutable release identity and verifies every
  downloaded asset digest before installation.

Deployment is intentionally separate from publication. Creating a GitHub
Release does not install binaries, restart services, or change the TV input.

## Configuration and Network Boundary

Start from the committed example files only. Keep the real files local:

```text
deploy/inventory.example.ini      -> deploy/inventory.ini
deploy/group_vars/all.example.yml -> deploy/group_vars/all.yml
```

The default deployment uses UDP port `4243` for Lan Mouse input transport and
authenticated TCP port `8765` for controller requests. Firewall policy for the
current Linux deployment host remains operator-owned.

Do not commit passwords, controller tokens, certificate fingerprints, TV
addresses, display identifiers, or real inventory. The repository ignore rules
cover the standard local configuration and generated native-build artifacts.

## Releases

One release tag identifies the workspace source and complete native artifact
set. Default GTK archives remain available alongside no-GTK Linux, Windows, and
macOS assets. The initial controller asset is Linux x86_64 only. Every archive
contains applicable license and README material, and
`osswitch-release-manifest.json` binds the release ID, tag commit, names, sizes,
and SHA-256 digests.

Release automation creates and verifies a draft before publication. It does not
deploy binaries or restart hosts.

- A manually dispatched `staging-*` tag produces a prerelease.
- Pushing a `v*` tag produces a final release.
- Staging and final tags may point to the same commit so the final release
  rebuilds the exact source that passed staging.

Release tags are immutable publication identities. A source change requires a
new staging run and a new final version tag.

## Engineering Documentation

- [Display-switch implementation plan](docs/plan_main_fullscreen_multiview_switch_implementation.md)
- [Atomic Lan Mouse input gate](docs/plan_lan_mouse_atomic_input_gate.md)
- [Clipboard handoff plan](docs/clipboardplan.md)
- [Protocol compatibility design](docs/protocolcompatibilitydesign.md)
- [TLA+ models](tla/README.md)

## Contributing

Keep behavioral changes focused and add tests for the invariant being changed.
Before submitting a change, run the applicable subset of:

```bash
cargo check --locked --workspace
cargo test --locked --workspace
python3 -m unittest discover -s tests -v
python3 -m unittest discover -s deploy/tests -v
```

Changes to deployment should also pass an Ansible syntax check with the
sanitized example inventory. Never use a real inventory in CI.

## License and Attribution

The repository's original osswitch components are licensed under
GPL-3.0-or-later; see `LICENSE`. The imported Lan Mouse history, attribution,
and component license are retained under `lan-mouse/`. Third-party dependencies
remain governed by their own licenses.
