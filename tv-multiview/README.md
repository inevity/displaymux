# tv-multiview

`tv-multiview` is the controller daemon for an osswitch installation. It
maintains the LG WebOS SSAP session, observes display state and signal presence,
coordinates two-phase Lan Mouse switch requests, and converges failures to the
configured Lan Mouse server host.

The architecture colocates the daemon with that configured hub/server host; it
does not define Linux as the permanent controller owner. The current deployment
and release workflow package the controller service only for Linux. Controller
service integration on macOS and Windows is not yet packaged.

## Current support

- TV support is currently limited to LG OLED webOS televisions with multiple
  HDMI inputs and the required SSAP capabilities.
- Linux controller build, test, release, systemd deployment, and runtime paths
  are implemented.
- macOS and Windows controller operation is not currently supported: native CI,
  release artifacts, service supervision, and live validation are still
  missing. This is an implementation gap, not an architectural restriction.

Build and test it from the repository root:

```bash
cargo test --locked -p tv-multiview
cargo build --locked --release -p tv-multiview
```

Runtime configuration and service deployment are documented under `deploy/`.
The daemon is licensed under GPL-3.0-or-later as part of osswitch.
