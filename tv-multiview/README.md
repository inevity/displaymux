# tv-multiview

`tv-multiview` is the display-controller daemon for an osswitch installation. It
maintains the LG WebOS SSAP session, observes display state and signal presence,
coordinates two-phase Lan Mouse switch requests, and converges failures to the
configured Lan Mouse server host.

The architecture colocates the daemon with that configured hub/server host; it
does not define Linux as the permanent controller owner. The current deployment
and release workflow package the display-controller service only for Linux.
Display-controller service integration on macOS and Windows is not yet
packaged.

## Current display support

- The current display adapter supports only LG OLED webOS displays with
  multiple HDMI inputs and the required SSAP capabilities. The LG device is a
  display in the Osswitch domain, not the definition of the display role.
- Linux display-controller build, test, release, systemd deployment, and runtime
  paths are implemented.

## Required controller TODOs

- [ ] Add native macOS display-controller CI builds and tests.
- [ ] Add macOS display-controller service supervision, release assets, and live
  validation.
- [ ] Add native Windows display-controller CI builds and tests.
- [ ] Add Windows display-controller service supervision, release assets, and
  live validation.

Until this work is complete, macOS and Windows are Lan Mouse peer platforms but
not completed display-controller platforms. This is an implementation gap, not
an architectural restriction.

Build and test it from the repository root:

```bash
cargo test --locked -p tv-multiview
cargo build --locked --release -p tv-multiview
```

Runtime configuration and service deployment are documented under `deploy/`.
The daemon is licensed under GPL-3.0-or-later as part of osswitch.
