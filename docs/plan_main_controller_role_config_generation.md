# Controller-role configuration generation plan

## Objective

Make one host-assignment mapping the Ansible source of truth for generated
DisplayMux roles. Three distinct inventory hosts are assigned to controller,
left-client, and right-client roles independently of their operating systems.

## Scope

- Support exactly three inventory hosts in any Linux, macOS, or Windows mix.
- Assign one host as controller and the other two as left/right clients.
- Derive addresses, fingerprints, display mappings, and peer positions from
  stable inventory host identities and the configured role assignment.
- Render one hub configuration and two spoke configurations automatically.
- Render `tv-multiview/config.toml` on the selected controller host.
- Replace OS-named Rust protocol identities with controller/left/right roles.
- Preserve the current effective controller, left-client, and right-client
  topology after migrating the ignored configuration.
- Update connection checks and certificate-identity validation to use the
  assigned controller independently of platform.

## Non-goals

- Adding macOS or Windows `tv-multiview` service supervision.
- Moving controller binaries between hosts.
- Supporting more or fewer than three hosts.
- Changing the physical host layout.
- Exposing real inventory, tokens, addresses, or certificate fingerprints.

## Invariants

1. Exactly one generated Lan Mouse configuration is a hub configuration.
2. Exactly two generated Lan Mouse configurations are client configurations.
3. Controller, left, and right assignments name three distinct inventory hosts.
4. Platform groups select deployment tasks only; they never identify roles.
5. Every client configuration points to the assigned controller host.
6. The controller places the left client on its left edge and the right client
   on its right edge; each client uses the inverse edge to return.
7. Every `switch_controller.url` and controller `bind_address` uses the assigned
   controller host address.
8. Hub clients exclude the controller and include both clients exactly once.
9. Client trust authorizes the assigned controller fingerprint.
10. Changing only the host-assignment mapping regenerates all three roles.

## Implementation checklist

- [x] Replace the invalid OS-keyed plan with host assignments and platform-neutral roles.
- [x] Replace Rust OS-named switch identities with controller/left/right roles.
- [x] Validate exactly three distinct assigned inventory hosts.
- [x] Key host-specific settings by inventory host identity.
- [x] Generalize hub, client, and controller templates.
- [x] Select controller/client templates independently of platform task files.
- [x] Update connection checks and certificate-identity validation.
- [x] Migrate sanitized examples and the ignored current configuration.
- [x] Document the current three-host limit.
- [x] Run Rust, Ansible, and live deployment verification.
