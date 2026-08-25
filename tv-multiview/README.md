# tv-multiview

`tv-multiview` is the display-controller daemon for a DisplayMux installation.
It maintains the device-specific display session, observes display state and
signal presence, coordinates two-phase Lan Mouse switch requests, and converges
failures to the configured Lan Mouse server host.

The architecture colocates the daemon with that configured hub/server host; it
does not define Linux as the permanent controller owner. The current deployment
and release workflow package the display-controller service only for Linux.
Display-controller service integration on macOS and Windows is not yet
packaged.

## Current display support

- The current display adapter supports only LG webOS TVs, specifically OLED
  models with multiple HDMI inputs and the required SSAP capabilities. The TV
  is a display in the DisplayMux domain, not the definition of the display role.
- Linux display-controller build, test, release, systemd deployment, and runtime
  paths are implemented.
- Native controller check/test jobs are configured for Windows x86_64, macOS
  Intel, and macOS Apple Silicon.
- Native controller release archives are configured for Linux x86_64, Windows
  x86_64, macOS Intel, and macOS Apple Silicon.

## Controller-platform implementation progress

- [x] Add native macOS display-controller CI builds and tests.
- [x] Add native macOS display-controller release assets.
- [ ] Add macOS display-controller service supervision and live validation.
- [x] Add native Windows display-controller CI builds and tests.
- [x] Add native Windows display-controller release assets.
- [ ] Add Windows display-controller service supervision and live validation.

The macOS and Windows controller packages are available for manual use, but
those operating systems are not completed display-controller platforms until
their service supervision and live validation work is complete. This is an
implementation gap, not an architectural restriction.

Build and test it from the repository root:

```bash
cargo test --locked -p tv-multiview
cargo build --locked --release -p tv-multiview
```

Runtime configuration and service deployment are documented under `deploy/`.
The daemon is licensed under GPL-3.0-or-later as part of DisplayMux.
