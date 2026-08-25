# DisplayMux

DisplayMux coordinates keyboard/pointer ownership and display routing across a
set of connected hosts.

## Language

**Display**:
The visual output device whose selectable input route participates in a host
switch transaction.
_Avoid_: TV when referring to the general domain role

**Display adapter**:
The integration boundary for observing and controlling a particular family of
displays.
_Avoid_: TV support

**Display controller**:
The authority that coordinates display-route state with keyboard/pointer
ownership.
_Avoid_: Linux controller, TV controller

**Display route**:
The display input currently selected for a host.
_Avoid_: Display input ownership

**Keyboard/pointer ownership**:
The host currently receiving the user's keyboard and pointer events as one
atomic unit.
_Avoid_: Input owner, input ownership without qualification

**Controller host**:
The one configured host assigned the Lan Mouse hub/server and display-controller
roles.
_Avoid_: Linux host when referring to the architecture

**Client host**:
One of the two configured non-controller hosts. Each client host is placed to
the left or right of the controller host.
_Avoid_: macOS client or Windows client when referring to the role

**Host assignment**:
The configuration that assigns three distinct hosts to the controller, left
client, and right client roles.
_Avoid_: Using an operating system as a host identity

**Platform**:
The operating system that determines a host's native build, installation, and
service integration. Platform does not determine host identity or role.
_Avoid_: Linux, macOS, or Windows as a controller/client identifier
