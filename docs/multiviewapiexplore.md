# MultiView API Surface — LG WebOS (OLED G4, webOS v9)

## Research Summary

Exhaustive search across: bscpylgtv source + docs, ColorControl (Maassoft), RootMyTV, Home Assistant webOS integration, all known LG WebOS control projects.

## What the API Exposes

| Setting | Category | Access | Values | Endpoint |
|---|---|---|---|---|
| `multiViewStatus` | `option` | **Read-only** | `"on"` `"off"` | `ssap://settings/getSystemSettings` |
| `splitscreenEnable` | `commercial` | **Read/Write** | `"on"` `"off"` | `ssap://settings/setSystemSettings` |

## What the API Does NOT Expose

- **No input selection**: cannot specify *which* two HDMI sources go into the split
- **No layout control**: cannot choose left/right, top/bottom, PIP position
- **No audio source selection**: TV defaults to one of the two sources
- **No split ratio control**: TV uses default ratio

## Input Selection: Manual Only

The input pair for multiView is selected exclusively via the on-screen Quick Settings menu (remote: gear button → Multi View → choose two sources). The TV remembers the last-used configuration across power cycles. No SSAP or Luna endpoint exposes this configuration.

## Toggle API Usage

```python
# Enable side-by-side (uses last-configured input pair)
await client.set_system_settings("commercial", {"splitscreenEnable": "on"})

# Disable side-by-side (return to fullscreen)
await client.set_system_settings("commercial", {"splitscreenEnable": "off"})

# Read current status (subscription-based, live updates)
# Category: "option", key: "multiViewStatus"
# Fires callback with {"settings": {"multiViewStatus": "on"|"off"}}
await client.subscribe(on_change, ep.GET_SYSTEM_SETTINGS,
    payload={"category": "option", "keys": ["multiViewStatus"]})
```

## Projects Checked

| Project | multiView Support | Notes |
|---|---|---|
| bscpylgtv (chros73) | Read status only | `multiViewStatus` in settings catalog |
| ColorControl (Maassoft) | None | No multiView-related code |
| RootMyTV | None | No issues/discussions |
| aiopylgtv (bendavid) | None | Predecessor to bscpylgtv |
| homebridge-webos-tv | None | Only input switching, no multiView |
| Home Assistant webos integration | None | No multiView entity/service |
| pyLGTV (TheRealLink) | None | Original implementation |

## Implication

Full-auto cursor-edge-to-split-screen with specific inputs is not achievable with known public API. The best programmable experience is: pre-configure the desired input pair once via remote, then toggle sxs on/off programmatically. The daemon's existing approach (skip input switches when sxs is active) is the correct strategy.
