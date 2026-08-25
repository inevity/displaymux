# Controller-role configuration generation plan

## Objective

Make `lan_mouse_server_host` the single Ansible selector for generated
DisplayMux roles. The selected logical host receives the Lan Mouse hub and
display-controller configuration; the other hosts receive Lan Mouse spoke
configuration.

## Scope

- Accept `linux`, `mac`, or `windows` as `lan_mouse_server_host`.
- Derive inventory hosts, addresses, fingerprints, and peer positions from the
  logical role.
- Render one hub configuration and two spoke configurations automatically.
- Render `tv-multiview/config.toml` on the selected controller host.
- Preserve the current rendered behavior when `lan_mouse_server_host: linux`.
- Update connection checks and post-deployment trust reconciliation to use the
  selected hub where their platform modules allow it.

## Non-goals

- Adding macOS or Windows `tv-multiview` service supervision.
- Moving controller binaries between hosts.
- Changing the DisplayMux protocol, Rust runtime, or physical host layout.
- Exposing real inventory, tokens, addresses, or certificate fingerprints.

## Invariants

1. Exactly one generated Lan Mouse configuration is a hub configuration.
2. Every other generated Lan Mouse configuration points to that selected hub.
3. Every `switch_controller.url` and controller `bind_address` uses the selected
   host address.
4. Hub clients exclude the hub itself and include every spoke exactly once.
5. Spoke trust authorizes the selected hub fingerprint.
6. Changing only `lan_mouse_server_host` changes the generated roles.
7. The `linux` selector preserves the existing effective topology.

## Implementation checklist

- [x] Validate the selector and one-host-per-role inventory shape.
- [x] Define shared logical-role mappings in the deployment play.
- [x] Generalize hub, spoke, and controller templates.
- [x] Select hub/spoke templates in Linux, macOS, and Windows tasks.
- [x] Generate the controller configuration on the selected host.
- [x] Remove or explicitly gate configuration checks tied to the Linux service.
- [x] Add Ansible-native selector and topology validation.
- [x] Update sanitized examples and usage documentation.
- [x] Run Ansible-native validation, syntax checks, and source diff validation.
