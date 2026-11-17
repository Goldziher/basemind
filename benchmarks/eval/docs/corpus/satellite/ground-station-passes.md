# Mirelle-1 Ground Station Passes

## Ground stations

The primary station is Hollin Ridge, which has a 3.7 m dish for S-band downlink and a
crossed-yagi antenna for UHF commanding. The backup station at Saltmarsh Point has UHF only,
plus time on a 2.4 m dish shared with the university's radio astronomy group.

## Pass prediction

Orbital elements are refreshed every morning from tracking data and the scheduler then
computes the next three days of contacts. A pass is only scheduled if it rises higher than
10 degrees above the horizon; lower passes suffer from terrain masking and noise. Hollin
Ridge sees about six usable passes a day, each offering seven to nine minutes of contact
above that mask.

## Downlink budget

The S-band link runs at 1 Mbit/s. After protocol overheads and the time needed to acquire
lock at the start of each pass, about 380 MB of image data can be brought down per day. This
is the figure the imaging planners use when deciding how many lagoons to photograph.

## Commanding windows

Command uploads are made only on passes that peak above 25 degrees, where the link margin is
comfortable. Every upload needs two people: an operator who sends the plan and a flight
director who checks it against the approved schedule. Command plans are frozen two hours
before the pass.

## Anomalies during a pass

If lock is lost for more than 30 seconds, the operator stops the downlink, notes the time,
and requests a repeat of the missing files on the next pass. Two consecutive failed passes
are escalated to the flight director on call.
