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
The configured hub/server host that owns the display-controller role.
_Avoid_: Linux host when referring to the architecture
