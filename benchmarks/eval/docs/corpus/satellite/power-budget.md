# Mirelle-1 Power Budget

Mirelle-1 is a six-unit CubeSat that images coastal lagoons. This document sets out how much
electrical power the spacecraft generates and how much each operating mode consumes.

## Solar array

Four body-mounted panels and two deployable wings carry 28 per cent efficient triple-junction
cells. With the wings deployed and the spacecraft sun-pointing, the array produces up to
21.4 W at beginning of life. In the nadir-pointing attitude used for imaging the average
falls to about 11 W over the sunlit part of the orbit. A maximum power point tracker on each
string keeps the cells near their optimum voltage as temperature changes.

## Battery

The battery pack has four lithium-ion cells in a 2S2P arrangement storing 38 Wh. To give the
cells a long life, the flight software keeps depth of discharge below 25 per cent in normal
operations; if the state of charge falls under 65 per cent the spacecraft enters safe mode
automatically. A film heater keeps the pack above 5 degrees Celsius.

## Operating modes

- Safe mode: 3.1 W for the radio beacon, the on-board computer and survival heaters.
- Nominal mode: 7.8 W, adding fine attitude control and housekeeping telemetry.
- Imaging mode: 14.6 W, adding the multispectral imager and on-board compression.
- Downlink mode: 12.2 W, with the S-band transmitter at full output.

## Eclipse and margins

The orbit period is 94.6 minutes and the longest eclipse lasts 35.8 minutes. The budget is
built on the worst case of a full eclipse spent in nominal mode, followed by a sunlit arc
that must both recharge the battery and carry the payload. The power subsystem requires an
orbit-average margin of at least 15 per cent at end of life, when cell degradation is
expected to have reduced array output by about 9 per cent.

## Change control

Any new load above 0.5 W, or any change to the schedule of imaging and downlink modes, must
be approved by the power subsystem lead before it is uploaded.
