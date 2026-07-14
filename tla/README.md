# TvDisplaySwitch TLC Artifacts

`TvDisplaySwitch.tla` is mechanically extracted from the TLA+ code block in
`../docs/fullscreenmultiviewswitchdesign.md`. Keep the two copies identical.

`TvDisplaySwitch.cfg` assigns the current candidate production timer values.
The specification remains infinite-state because request, switch, and reconnect
epochs are unbounded, so exhaustive TLC does not terminate with this config.

`TvDisplaySwitchFinite.cfg` keeps both remote hosts, reduces timer magnitudes,
and applies `TLCFiniteState`. It covers one request and up to two switch epochs,
including a remote attempt followed by server-host fallback. Passing this model
is bounded validation, not proof of the unbounded specification.

Run the finite check from this directory:

```sh
java -XX:+UseParallelGC \
  -cp /home/example/.cache/nvim/tla.nvim/tla2tools.jar \
  tlc2.TLC -cleanup -workers 4 -lncheck final \
  -config TvDisplaySwitchFinite.cfg TvDisplaySwitch.tla
```

`tlc-pre-fix/` preserves every `TvDisplaySwitch*` source/configuration moved
from `/tmp` after the first checker run. Those files document the parser,
safety, and liveness failures and are not canonical runnable modules.

## Latest Completed Check

TLC 2.19 completed `TvDisplaySwitchFinite.cfg` on 2026-07-14 after the
edge-intent protocol was added, with no error:

- 308,009,681 states generated
- 8,717,850 distinct states
- depth 34
- all twelve invariants checked
- all four liveness properties checked

The corrected model also requires first edge contact, backend retreat evidence,
and a second matching contact before reservation, wake, TV control, readiness
rejection, or MultiView ownership can start. It explicitly handles
server-signal loss during wake timeout, cancels wake state on subscription
override and SSAP disconnect, and uses strong fairness for timeout versus
readiness retry under an oscillating environment.
