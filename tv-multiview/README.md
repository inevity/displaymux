# tv-multiview

`tv-multiview` is the Linux controller service for an osswitch installation. It
maintains the LG WebOS SSAP session, observes display state and signal presence,
coordinates two-phase Lan Mouse switch requests, and converges failures to the
configured Lan Mouse server host.

Build and test it from the repository root:

```bash
cargo test --locked -p tv-multiview
cargo build --locked --release -p tv-multiview
```

Runtime configuration and service deployment are documented under `deploy/`.
The daemon is licensed under GPL-3.0-or-later as part of osswitch.
