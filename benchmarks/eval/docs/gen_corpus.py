#!/usr/bin/env python3
"""Generate the docs_search evaluation corpus and its task file.

Stdlib only and deterministic: re-running writes byte-identical files. Every
document is original text about invented subjects. Output:

  corpus/      the documents, scanned as their own workspace
  tasks.jsonl  the evaluation tasks, gold paths relative to corpus/
"""

from __future__ import annotations

import json
import shutil
import textwrap
import zipfile
from email import message_from_string
from pathlib import Path
from xml.sax.saxutils import escape as xml_escape

HERE = Path(__file__).resolve().parent
CORPUS = HERE / "corpus"
TASKS = HERE / "tasks.jsonl"

# The MIME type the extractor reports per extension. Correct a value here and
# rerun the script: tasks.jsonl is regenerated from it.
MIME_BY_EXT = {
    ".md": "text/markdown",
    ".html": "text/html",
    ".pdf": "application/pdf",
    ".docx": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ".eml": "message/rfc822",
    ".ipynb": "application/x-ipynb+json",
    ".txt": "text/plain",
    ".log": "text/plain",
    ".csv": "text/csv",
    ".json": "application/json",
    ".yaml": "application/yaml",
    ".toml": "application/toml",
}

ZIP_EPOCH = (1980, 1, 1, 0, 0, 0)
TASK_K = 10

# Plain text of every generated document, keyed by corpus-relative path, used
# to check that a rare-term task's term occurs in exactly one document.
PLAIN: dict[str, str] = {}


def write_text(rel: str, text: str, plain: str | None = None) -> None:
    path = CORPUS / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(text.encode("utf-8"))
    PLAIN[rel] = plain if plain is not None else text


def dedent(s: str) -> str:
    return textwrap.dedent(s).lstrip("\n")


# --------------------------------------------------------------------------
# Markdown
# --------------------------------------------------------------------------

MARKDOWN = {
    "utility/pumping-stations.md": """
        # Varnholt Water Board: Pumping Station Operations

        This note describes how the Varnholt Water Board moves water from the Elske River to the taps of
        roughly 140,000 residents. It is written for shift operators and for engineers joining the
        operations team.

        ## Network overview

        Raw water enters the system at the Ostbank intake, where pumping station P-7 lifts it from the
        river channel into the screening house. From there it travels through the Millrace treatment
        works, is stored at the Tern Hill reservoir, and reaches the city by gravity. Three further
        stations support the network: Quayside, which serves the low-lying harbour district; Hollow
        Lane, a booster on the eastern ridge; and Tern Hill itself, which tops up the high-level tank
        that feeds the hospital and the northern estates.

        ## Raw water path

        The route raw water takes depends on how cloudy the river is when it arrives. Operators watch
        the turbidity meter at the screening house, because the filters at Millrace clog quickly when
        the Elske carries silt after heavy rain on the uplands. The flowchart below is the routing
        logic the control system follows.

        ```mermaid
        flowchart LR
            A[Elske intake] -->|raw water| B(Ostbank P-7)
            B -->|screened| C{Turbidity above 4 NTU?}
            C -->|yes| D[Settling lagoon]
            C -->|no| E[Millrace treatment works]
            D -->|after 6 hours| E
            E -->|treated| F[Tern Hill reservoir]
            F -->|gravity main| G[City districts]
        ```

        When the reading stays above the threshold, water is diverted into the settling lagoon beside
        the intake and held for at least six hours before it is released to Millrace. The lagoon holds
        about one and a half days of average demand, so a long spell of muddy river water forces the
        board to draw down Tern Hill and to ask large customers to cut back. Operators must log every
        diversion, with start and end times, in the station diary.

        ## Duty and standby rotation

        Every station has at least one duty pump and one standby pump. To keep running hours even, the
        pumps swap roles each Monday at 08:00 during the morning handover. The outgoing shift records
        the hour-meter readings and the incoming shift confirms the standby set starts cleanly before
        the swap is accepted. A pump that fails to start is tagged out and reported to the mechanical
        fitters on the same day.

        ## Telemetry and alarms

        All five stations report to the central SCADA system over leased lines, with a radio link as
        backup. High-priority alarms (loss of suction, motor overtemperature, reservoir low-low level)
        page the on-call engineer directly. In 2024 the board's R&amp;D group fitted acoustic leak
        loggers to the trunk main between Millrace and Tern Hill; the trial found two leaks in its
        first month, and the loggers are now being extended across the network under the joint
        R&amp;D programme with the regional university.

        ## Energy use

        Pumping is the board's largest electricity cost. Tern Hill is filled mainly during the night
        tariff window between 23:00 and 06:00, and daytime pumping is limited to what is needed to
        hold the reservoir above forty per cent.
    """,
    "utility/billing-policy.md": """
        # Varnholt Water Board: Billing Policy

        ## Billing cycle

        Customers are billed quarterly. Meters are read in the final three weeks of each quarter by
        field readers or, for properties with smart meters, automatically at midnight on the last day
        of the quarter. Where a reading cannot be obtained, the bill is estimated from the average
        daily use over the previous four quarters and marked "estimated" on the front page. Two
        estimated bills in a row trigger a visit to check the meter.

        ## Tariff schedule

        Each account is assigned a tariff code that sets the fixed charge and the price of each cubic
        metre.

        | Code | Customer class | Fixed charge per quarter | Rate per m3 | Notes |
        |------|----------------|--------------------------|-------------|-------|
        | WB-11 | Domestic standard | 24.00 | 1.62 | Default for households |
        | WB-12 | Domestic assisted | 9.00 | 1.10 | Requires an annual income check |
        | WB-21 | Small commercial | 41.00 | 1.75 | Shops and offices under 500 m3 a year |
        | WB-22 | Hospitality | 58.00 | 1.80 | Hotels, cafes and restaurants |
        | WB-31 | Industrial | 120.00 | 1.48 | Volume above 20,000 m3 a year |
        | WB-41 | Agricultural irrigation | 35.00 | 0.92 | Seasonal, April to September only |
        | WB-51 | Fire service standby | 15.00 | 0.00 | Sprinkler connections, metered for audit |
        | WB-61 | Temporary construction | 30.00 | 2.10 | Standpipes, maximum twelve months |
        | WB-71 | Community allotments | 6.00 | 0.85 | Registered garden associations |

        Prices are in Varnholt crowns and exclude the sewerage charge, which is billed separately.

        ## Hardship and payment plans

        A household that cannot pay a bill in full may ask to spread it over up to twelve monthly
        instalments without interest. Customers on WB-12 who fall behind are contacted by a support
        officer before any recovery letter is sent. Supply to a domestic property is never cut off
        because of unpaid charges.

        ## Leak allowances

        If a customer's use rises sharply because of a concealed leak on their own pipework, the board
        will refund half of the excess above their normal consumption once the leak has been repaired
        by a licensed plumber. Only one allowance is granted in any 24-month period, and the claim
        must be made within two quarters of the bill concerned.

        ## Disputes

        A customer who disagrees with a bill should first contact the billing team. If the matter is
        not resolved within 28 days, it can be referred to the independent Consumer Water Panel,
        whose decision is binding on the board.
    """,
    "utility/water-quality-report.md": """
        # Annual Drinking Water Quality Summary 2025

        ## Sampling programme

        During 2025 the board's laboratory analysed 1,240 samples taken from customer taps, service
        reservoirs and the outlet of the Millrace treatment works. Sampling points are chosen at
        random within each supply zone each month, with fixed points at schools and care homes. The
        programme is audited every year by the regional health inspectorate.

        ## Results at a glance

        Of the 18,600 individual tests carried out, 99.96 per cent met the national standard. The
        eight failures were all for coliform bacteria at single taps and none was confirmed on
        resampling, which points to contamination of the tap fittings rather than of the network.
        Free chlorine at the far ends of the network averaged 0.21 mg/L, comfortably above the
        0.1 mg/L minimum the board sets for itself.

        ## Taste and odour episode

        In late August customers in several districts reported an earthy, musty taste. The cause was
        geosmin produced by a bloom of blue-green algae in a slow reach of the Elske above the intake
        during a fortnight of hot, still weather. The compound is harmless at these concentrations,
        but people notice it at a few nanograms per litre. Powdered activated carbon was dosed at
        Millrace for nineteen days until the bloom collapsed, and complaints stopped within a week of
        dosing starting.

        ## Lead service lines

        Older terraces in Cooper's Row and the Weir Bridge quarter still have lead pipes between the
        main and the house. The board replaced 312 of these connections during the year, bringing
        the total since the programme began to 1,905. Orthophosphate dosing continues while the
        remaining 640 are replaced, and households in affected streets can ask for a free lead test
        at any time.

        ## Looking ahead

        The 2026 programme adds monthly testing for PFAS compounds at both intakes and extends online
        chlorine monitoring to the Hollow Lane booster.
    """,
    "game/rules-overview.md": """
        # Cairnfall: Rules Overview

        Cairnfall is a tile-laying game for two to four players about building stone cairns on a
        windswept upland. A game lasts about an hour.

        ## Components

        The box contains 60 hexagonal terrain tiles (moor, scree, bog and ridge), 48 cairn stones in
        four colours so each player has 12, 8 sigil tiles, a weather deck of 24 cards, a scoring
        track and four reference cards.

        ## Setup

        Shuffle the terrain tiles face down and place the starting tile, the single tile showing a
        standing stone, in the middle of the table. Each player draws three terrain tiles as a hand.
        Shuffle the weather deck and turn the top card face up beside it as the current weather. The
        player who most recently walked uphill goes first.

        ## Turn sequence

        A turn has four phases, always taken in the same order: draw, place, raise and weather.
        Players may not skip a phase, although some phases will have no effect.

        In the draw phase, take one terrain tile from the face-down stack so you hold four. If the
        stack is empty, shuffle the discards to form a new one; if there are no discards either, play
        continues with whatever is in hand.

        In the place phase, put one tile from your hand into the landscape so that at least one edge
        touches a tile already on the table. Edges do not need to match terrain types, but placing a
        tile so that its ridge edge meets another ridge edge extends a ridge line, and ridge lines
        matter for scoring. A bog tile may never be placed directly next to the starting tile. If you
        cannot legally place any tile, discard one and draw a replacement instead; your turn then
        continues with the raise phase.

        In the raise phase, you may put one of your cairn stones onto any tile that has no stone of
        another colour, or onto a tile that already carries your own stones to make your cairn
        taller. A cairn can be at most four stones high. Stones may not be placed on bog tiles at
        all, and on scree a cairn cannot be taller than two. If you raise a stone on a tile adjacent
        to a sigil tile, you may take that sigil as long as no other player already holds it; sigils
        give bonus points at the end and some allow a one-off special action, as printed on the tile.

        In the weather phase, the active player turns over the top weather card and resolves it
        immediately. Fair weather has no effect. Wind removes the top stone from every cairn of
        height four. Fog stops everyone from claiming sigils until the next weather card is turned.
        Rain turns one moor tile of the active player's choice into bog by placing a bog marker on
        it, which may strand stones already on that tile; stranded stones stay where they are but no
        longer count towards cairn height for scoring. Snow freezes ridge lines, so no ridge can be
        extended during the next round. When the weather deck runs out, shuffle all weather cards
        back together and start again; the second time the deck runs out triggers the end of the
        game.

        Play passes to the left after the weather phase. New players often forget the weather phase
        entirely during their first few turns, so it helps to keep the weather deck on the side of
        the table nearest to the person whose turn it is, and to pass it along as play moves round.

        Experienced groups sometimes resolve the draw phase of the next player while the current
        player is still deciding where to raise a stone. This is allowed, provided the next player
        keeps the drawn tile face down until the weather card for the current turn has been
        resolved, because snow and rain can change which placements are worth making.

        ## End of game

        The game ends immediately when the weather deck runs out for the second time, or at the end
        of the turn in which a player places their last cairn stone. Players then score according to
        the scoring reference card, and the player with the most points wins. Ties are broken in
        favour of the player holding more sigils.
    """,
    "game/tournament-policy.md": """
        # Cairnfall Open: Tournament Policy

        This policy applies to the annual Cairnfall Open and to qualifying events run by affiliated
        clubs.

        ## Format

        The Open is played over five Swiss rounds at tables of four. Players are seated by standings
        after the first round, which is drawn at random. Tables of three are used only when the
        number of entrants is not divisible by four, and players at a table of three receive a
        scoring adjustment of plus five points.

        ## Time control

        Each round lasts 75 minutes. When the judge calls time, the table finishes the current round
        of turns so that every player has had the same number of turns, then scores the game as if
        it had ended normally. Players who are visibly stalling to run out the clock are dealt with
        under the conduct rules below.

        ## Tiebreakers

        Final standings are ordered by match points: 4 for a table win, 2 for second place and 1 for
        third. Ties on match points are broken first by total game points across all rounds, then by
        the number of sigils claimed, and finally by the average match points of the opponents each
        player has faced.

        ## Conduct and penalties

        Slow play receives a warning for a first offence and a game loss for a second within the same
        event. Deliberately misplacing tiles or stones, or concealing sigils, results in
        disqualification. Players are expected to call a judge rather than resolve disputes among
        themselves.

        ## Appeals

        A ruling by a floor judge can be appealed to the head judge before the next round begins.
        The head judge's decision is final and is recorded in the event log.

        ## Equipment

        Players bring their own copy of the game unless told otherwise. Sleeves are not needed, as
        no cards are held in hand, but weather decks must be complete and unmarked.
    """,
    "satellite/power-budget.md": """
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
    """,
    "satellite/ground-station-passes.md": """
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
    """,
    "misc/sourdough-starter.md": """
        # Keeping a Rye Sourdough Starter

        ## What a starter is

        A sourdough starter is a paste of flour and water in which wild yeasts and lactic acid
        bacteria live together. Rye flour gives a lively starter because it carries more of these
        organisms and more of the sugars they feed on than white wheat flour does.

        ## Feeding schedule

        At room temperature a starter should be fed every twelve hours. Discard all but one spoonful,
        then add flour and water by weight in the ratio one part starter, two parts flour and two
        parts water. A starter kept in the refrigerator needs feeding only once a week; take it out
        the evening before you want to bake and give it two feeds at room temperature first.

        ## Signs of trouble

        A layer of grey liquid on top means the starter is hungry; pour it off and feed. A sharp smell
        like nail varnish means the same. Pink or orange streaks, or fuzzy mould of any colour, mean
        the culture has been contaminated, and the whole jar should be thrown away.

        ## Reviving a neglected starter

        A starter forgotten at the back of the refrigerator for a month or two can usually be saved.
        Scrape away the top layer, take a teaspoon from underneath and feed it three times over a day
        and a half. If it doubles within six hours after the third feed, it is ready to use again.

        ## Baking day timing

        Mix the dough when the starter has roughly doubled and the surface is domed and bubbly. In a
        warm kitchen that is four to six hours after feeding; in a cold one it may take ten.

        ## Flour choices

        Wholegrain rye is the most forgiving flour for a starter. Light rye works but rises more
        slowly, and a starter switched to white wheat flour needs a few days of feeds to settle. Avoid
        bleached flour, which carries fewer of the organisms the culture depends on.
    """,
    "misc/community-garden.md": """
        # Fennick Road Community Garden: Plot Rules

        ## Plot allocation

        The garden has 42 plots. New members join a waiting list and are offered plots in the order
        they applied. In their first year a new member is given a half plot, and may move to a full
        plot after one season if they have kept the half plot cultivated. Members who leave a plot
        untended for eight weeks receive a reminder and, after a further four weeks, lose the plot.

        ## Water use

        The garden buys its water from the city on the reduced community allotments tariff, which
        depends on the association keeping consumption reasonable. Hoses may not be used in July and
        August between 10:00 and 18:00; use watering cans during those hours. Every plot holder is
        encouraged to fit a water butt to the shared shed gutters.

        ## Compost and green waste

        The three-bay compost system by the gate takes vegetable waste, grass cuttings and shredded
        prunings. Diseased plants, perennial weed roots and cooked food go in the green waste skip.

        ## Shared tools and shed

        Tools in the shed are for everyone. Clean them before putting them back and report any
        breakages in the shed book. The rotavator may only be used by members who have been shown how
        by a committee member.

        ## Leaving a plot

        Members leaving the garden should clear their plot of structures they built, return the shed
        key, and tell the plot secretary so the next person on the list can be contacted.

        ## Work mornings

        The committee runs a work morning on the first Saturday of each month from March to October.
        Members are asked to attend at least three a year to help with paths, hedges and the shared
        fruit cage; those who cannot may pay a small fee towards hired help instead.
    """,
}


def build_markdown() -> None:
    for rel, body in MARKDOWN.items():
        write_text(rel, dedent(body))


# --------------------------------------------------------------------------
# HTML
# --------------------------------------------------------------------------


def html_page(title: str, body: str) -> str:
    return (
        '<!DOCTYPE html>\n<html lang="en">\n<head>\n<meta charset="utf-8">\n'
        f"<title>{title}</title>\n</head>\n<body>\n{dedent(body)}</body>\n</html>\n"
    )


def html_table(header: list[str], rows: list[list[str]]) -> str:
    out = ["<table>", "<thead>", "<tr>" + "".join(f"<th>{h}</th>" for h in header) + "</tr>", "</thead>", "<tbody>"]
    for row in rows:
        out.append("<tr>" + "".join(f"<td>{c}</td>" for c in row) + "</tr>")
    out += ["</tbody>", "</table>"]
    return "\n".join(out) + "\n"


HTML = {
    "utility/outage-notice.html": (
        "Planned supply interruption: Millrace trunk main",
        """
        <h1>Planned supply interruption: Millrace trunk main replacement</h1>
        <p>The Varnholt Water Board is replacing 600&nbsp;metres of cast-iron trunk main between the
        Millrace treatment works and Canal Street. The old main dates from 1911 and has burst four
        times in the last five years. While the new pipe is connected, some streets will be without
        water or will have low pressure.</p>
        <h2>Affected streets</h2>
        """
        + html_table(
            ["Street", "Date", "Interruption window", "Expected effect"],
            [
                ["Canal Street", "3 March", "09:00 to 15:00", "No supply"],
                ["Tanner&#39;s Yard", "3 March", "09:00 to 15:00", "No supply"],
                ["Lockkeeper&#39;s Walk", "4 March", "08:00 to 13:00", "No supply"],
                ["Ropewalk Lane", "4 March", "08:00 to 18:00", "Low pressure (&lt; 1.5 bar)"],
                ["Fennick Road, odd numbers", "5 March", "10:00 to 14:00", "No supply"],
                ["Fennick Road, even numbers", "5 March", "14:00 to 18:00", "No supply"],
                ["Mill Street", "3 to 5 March", "All day", "Low pressure, possible discoloured water"],
            ],
        )
        + """
        <h2>What you should do</h2>
        <p>Fill a kettle and a few bottles before the start time. Do not run washing machines or
        dishwashers during the window. When supply returns, run the cold kitchen tap until the water
        is clear; brief discolouration is normal &amp; harmless.</p>
        <h2>Help for vulnerable customers</h2>
        <p>Customers on the priority services register will have bottled water delivered to the door
        if the interruption is expected to last &gt; 6&nbsp;hours. To join the register, call the
        customer line or ask a neighbour to call on your behalf.</p>
        <h2>Why this work matters</h2>
        <p>Replacing the main will cut leakage, reduce bursts on the water &amp; sewer network in the
        canal quarter, and let the board raise pressure to the upper floors of the new flats on
        Canal Street.</p>
        """,
    ),
    "game/scoring-reference.html": (
        "Cairnfall scoring reference",
        """
        <h1>Cairnfall scoring reference</h1>
        <p>Use this sheet once the game has ended. Score each category in the order listed &amp;
        move your marker along the scoring track as you go, so nothing is counted twice.</p>
        <h2>Scoring table</h2>
        """
        + html_table(
            ["Scoring event", "Points", "Notes"],
            [
                ["Cairn of height 1", "1", "&nbsp;"],
                ["Cairn of height 2", "2", "&nbsp;"],
                ["Cairn of height 3", "5", "Not possible on scree"],
                ["Cairn of height 4", "8", "Height &gt; 2 is never allowed on scree"],
                ["Ridge line of 3 or 4 tiles", "3", "Each line counted once"],
                ["Ridge line of 5 or more tiles", "6", "Each line counted once"],
                ["Each sigil held", "2", "Including sigils claimed in fog-free turns only"],
                ["Longest ridge line", "4", "Split between tied players, rounded down"],
                ["Stranded stone on bog", "0", "Does not count towards height"],
            ],
        )
        + """
        <h2>The Quarry rule</h2>
        <p>When two or more cairns of the same colour stand on neighbouring tiles, together they form
        a quarry. Only the tallest cairn in a quarry scores its full height value; every other cairn
        in it scores 1&nbsp;point regardless of height. The rule stops players from packing clusters
        of medium cairns into one corner of the landscape.</p>
        <h2>Worked example</h2>
        <p>Maren holds two sigils, has a cairn of height 4 and a cairn of height 3 on neighbouring
        tiles, and owns a ridge line of five tiles. Her two cairns form a quarry, so she scores 8 for
        the taller one and 1 for the other, 6 for the ridge line and 4 for her sigils: 19 points in
        total, &lt; the 22 she would have scored had the cairns stood apart.</p>
        """,
    ),
    "satellite/thermal-control.html": (
        "Mirelle-1 thermal control",
        """
        <h1>Mirelle-1 thermal control</h1>
        <p>The thermal design keeps every unit of the spacecraft within its limits through a mix of
        surface finishes, insulation and a small number of heaters. It is deliberately simple:
        there are no louvres, heat pipes or pumped loops on board.</p>
        <h2>Passive control</h2>
        <p>Radiator faces carry white silicone paint &amp; the remaining structure is black
        anodised to spread heat evenly. A multi-layer insulation (MLI) blanket of fifteen layers
        wraps the battery pack and the barrel of the imager optics, isolating them from the rapid
        swings of the outer panels as the spacecraft enters and leaves eclipse.</p>
        <h2>Heaters</h2>
        <p>Two Kapton film heaters are fitted. THERM-HTR-01 sits on the battery and is driven by the
        power system&#39;s own thermostat. THERM-HTR-02 sits on the imager focal plane and is
        switched by the flight software: on when the detector reads &lt; -10&nbsp;&deg;C and off again
        once it reaches -5&nbsp;&deg;C. Each heater draws 1.2&nbsp;W.</p>
        <h2>Temperature limits</h2>
        """
        + html_table(
            ["Component", "Operating min (C)", "Operating max (C)", "Survival min (C)", "Survival max (C)"],
            [
                ["Battery pack", "5", "35", "-10", "45"],
                ["On-board computer", "-20", "70", "-40", "85"],
                ["Imager detector", "-10", "30", "-25", "50"],
                ["S-band transmitter", "-20", "55", "-30", "70"],
                ["Reaction wheels", "-10", "50", "-20", "60"],
                ["Solar panels", "-80", "100", "-100", "120"],
            ],
        )
        + """
        <h2>Analysis cases</h2>
        <p>The hot case assumes a beta angle of 70&nbsp;degrees, end-of-life surface properties and
        continuous downlink. The cold case assumes the longest eclipse, beginning-of-life surfaces
        and safe mode. In both cases every unit stays within its operating range with a margin
        &gt; 5&nbsp;&deg;C.</p>
        """,
    ),
    "misc/library-hours.html": (
        "Harrowgate Lane Library",
        """
        <h1>Harrowgate Lane Library</h1>
        <p>Welcome to your neighbourhood library. Membership is free for everyone who lives, works or
        studies in the borough &amp; you can join at the front desk with any proof of address.</p>
        <h2>Opening hours</h2>
        """
        + html_table(
            ["Day", "Opens", "Closes"],
            [
                ["Monday", "10:00", "19:00"],
                ["Tuesday", "10:00", "19:00"],
                ["Wednesday", "10:00", "13:00"],
                ["Thursday", "10:00", "19:00"],
                ["Friday", "10:00", "17:00"],
                ["Saturday", "09:30", "16:00"],
                ["Sunday", "Closed", "Closed, except the first Sunday of each month, 11:00 to 15:00"],
            ],
        )
        + """
        <h2>Borrowing &amp; renewals</h2>
        <p>Members may borrow up to 15 items for three weeks. Items can be renewed twice online or at
        the desk unless another member has reserved them. There are no fines for late returns, but
        borrowing is paused while any item is more than four weeks overdue.</p>
        <h2>Study room</h2>
        <p>The upstairs study room seats twelve and can be booked in two-hour slots, up to a week in
        advance. Groups of &gt; 6 people should book the meeting room instead.</p>
        <h2>Accessibility</h2>
        <p>The building has step-free access from the car park, a lift to the first floor and a
        hearing loop at the front desk. Large-print and audio books are shelved on the ground
        floor&nbsp;next to the entrance.</p>
        """,
    ),
}


def html_plain(text: str) -> str:
    import html
    import re

    return html.unescape(re.sub(r"<[^>]+>", " ", text))


def build_html() -> None:
    for rel, (title, body) in HTML.items():
        page = html_page(title, body)
        write_text(rel, page, html_plain(page))


# --------------------------------------------------------------------------
# PDF (hand-written PDF 1.4, Helvetica Type1)
# --------------------------------------------------------------------------

PDF_LINE_WIDTH = 88
PDF_TOP = 742
PDF_LEADING = 14
PDF_MAX_LINES = 46

PDFS = {
    "utility/flood-response.pdf": (
        "Varnholt Water Board Flood Response Procedure FRP-3",
        [
            [
                ("h", "Flood Response Procedure FRP-3"),
                (
                    "p",
                    "Issued by the Varnholt Water Board operations directorate. This procedure applies "
                    "whenever the Elske River threatens assets of the board, in particular the stations "
                    "that sit inside the mapped flood plain.",
                ),
                ("h", "1. Purpose and scope"),
                (
                    "p",
                    "The procedure sets out who decides, who acts and in what order when river levels "
                    "rise. It covers protection of pumping stations, continuity of supply, communication "
                    "with the public and the return to normal service. It does not cover sewer flooding, "
                    "which is handled by the drainage department.",
                ),
                ("h", "2. Trigger levels"),
                (
                    "p",
                    "Levels are read from river gauge E-114 at Weir Bridge, which reports every fifteen "
                    "minutes to the control room. Three alert stages are defined.",
                ),
                (
                    "p",
                    "Stage 1 (Watch) begins at 3.2 metres. The duty manager reviews the forecast every "
                    "hour and confirms that barrier crews are available.",
                ),
                (
                    "p",
                    "Stage 2 (Warning) begins at 3.8 metres. Barrier crews are mobilised and the "
                    "incident room is opened.",
                ),
                (
                    "p",
                    "Stage 3 (Emergency) begins at 4.4 metres, or earlier when the forecast shows the "
                    "river will pass that height within six hours. The operations director takes "
                    "command.",
                ),
                ("h", "3. Roles"),
                (
                    "p",
                    "The duty manager leads at Stages 1 and 2. The operations director, or a deputy named "
                    "on the weekly roster, leads at Stage 3. A communications officer joins the incident "
                    "room from Stage 2 onwards.",
                ),
            ],
            [
                ("h", "4. Station protection"),
                (
                    "p",
                    "Ostbank P-7 and Quayside are the two stations at risk. At Stage 2 crews fit the "
                    "demountable aluminium barriers at both sites. The barrier sections, posts and seals "
                    "are stored in the Quayside depot, and the target time to install them at both "
                    "stations is forty minutes from the crews arriving.",
                ),
                (
                    "p",
                    "At Stage 3 the electrical supply to the Quayside motor control centre is isolated "
                    "before water reaches the plinth, and the harbour district is fed from Hollow Lane "
                    "through the cross-connection valve on Mill Street. Ostbank keeps running unless "
                    "water enters the screening house, in which case the intake is closed and the "
                    "settling lagoon is drawn down to keep Millrace in service.",
                ),
                (
                    "p",
                    "Portable pumps are positioned inside each barrier line to remove seepage. Sandbags "
                    "are a last resort and are used only to close gaps the barriers cannot reach.",
                ),
                ("h", "5. Staffing"),
                (
                    "p",
                    "From Stage 2 each at-risk station is staffed continuously by two people. Staff must "
                    "wear lifejackets within ten metres of the water and must never enter flowing flood "
                    "water. Shifts are limited to twelve hours, with a relief crew called at least two "
                    "hours before handover.",
                ),
            ],
            [
                ("h", "6. Public notices"),
                (
                    "p",
                    "If flood water enters a treatment process or a service reservoir, or if pressure is "
                    "lost in a district for more than four hours, a precautionary boil-water notice is "
                    "issued for the affected area after consultation with the health inspectorate.",
                ),
                (
                    "p",
                    "Notices are published on the board's website, sent by text message to registered "
                    "customers, and delivered by hand to care homes and schools.",
                ),
                ("h", "7. Recovery"),
                (
                    "p",
                    "Before a flooded station returns to service it is cleaned, inspected by an "
                    "electrician and disinfected. Mains in the affected area are flushed and chlorinated. "
                    "A boil-water notice may be lifted only after two consecutive sets of samples, taken "
                    "at least 24 hours apart, are clear of indicator bacteria.",
                ),
                ("h", "8. Debrief"),
                (
                    "p",
                    "A debrief is held within ten working days of the stand-down. Its findings, with "
                    "named owners and dates, are added to the improvement log, and this procedure is "
                    "reviewed against them.",
                ),
            ],
        ],
    ),
    "game/rulebook-appendix.pdf": (
        "Cairnfall Rulebook Appendix",
        [
            [
                ("h", "Cairnfall Rulebook Appendix"),
                ("h", "Appendix A: Variant rules"),
                ("h", "A.1 Duel for two players"),
                (
                    "p",
                    "With two players, each player also takes four stones from an unused colour, which "
                    "count as their own for every purpose. Remove the fog cards from the weather deck "
                    "before shuffling, since claiming sigils is already less contested with two.",
                ),
                ("h", "A.2 Solo play against the Warden"),
                (
                    "p",
                    "A solo player competes against an automated opponent called the Warden. The Warden "
                    "has no hand. At the start of each of its turns, flip the top terrain tile and place "
                    "it touching the most recently placed tile, with its ridge edge facing the nearest "
                    "ridge line where possible. The Warden then raises a stone on the tile it just "
                    "placed, unless that tile is bog.",
                ),
                (
                    "p",
                    "You win if your score beats the Warden's score by at least ten points. The Warden "
                    "never claims sigils, but it scores two points for every weather card turned over "
                    "during its own turns.",
                ),
            ],
            [
                ("h", "Appendix B: Rulings and frequently asked questions"),
                (
                    "p",
                    "Q: Once a sigil has been claimed, can it be passed to another player or returned "
                    "to the landscape? A: No. A claimed sigil stays with the player who took it until "
                    "the end of the game, even if the cairn beside it is later stranded or blown down.",
                ),
                (
                    "p",
                    "Q: What happens to stones on a tile that rain turns into bog? A: They remain on the "
                    "tile but are stranded. Stranded stones score nothing and cannot be built upon.",
                ),
                (
                    "p",
                    "Q: Can a ridge line pass through the starting tile? A: Yes. The starting tile has "
                    "ridge edges on all six sides and may join any number of ridge lines.",
                ),
                (
                    "p",
                    "Q: Wind is drawn while snow is in effect. Which applies? A: Both. Wind removes "
                    "stones as usual; the frozen ridges stay frozen until the next round ends.",
                ),
                (
                    "p",
                    "Q: May a player look through the discard pile? A: Yes, at any time, but the order "
                    "of the discards must not be changed.",
                ),
            ],
        ],
    ),
    "satellite/mission-overview.pdf": (
        "Mirelle-1 Mission Overview",
        [
            [
                ("h", "Mirelle-1 Mission Overview"),
                ("h", "1. Objectives"),
                (
                    "p",
                    "Mirelle-1 will monitor harmful algal blooms in coastal lagoons and estuaries with a "
                    "six-band multispectral imager at a ground sampling distance of 30 metres. Its data "
                    "will give regional fisheries agencies an early warning of blooms that can close "
                    "shellfish beds, at a fraction of the cost of aerial surveys.",
                ),
                ("h", "2. Orbit"),
                (
                    "p",
                    "The spacecraft will fly in a sun-synchronous orbit at 520 km altitude with the "
                    "local time of the ascending node (LTAN) at 10:30. This gives consistent lighting "
                    "for the imager and revisits each target lagoon every three days.",
                ),
            ],
            [
                ("h", "3. Spacecraft bus"),
                (
                    "p",
                    "The bus is a six-unit aluminium structure carrying the payload, power, radio and "
                    "computing units described below.",
                ),
                ("h", "3.1 Attitude determination and control"),
                (
                    "p",
                    "Three reaction wheels and three magnetorquers control attitude, guided by a star "
                    "tracker, six coarse sun sensors and a magnetometer. Pointing accuracy is 0.5 "
                    "degrees in imaging mode, and the magnetorquers dump momentum from the wheels once "
                    "per orbit.",
                ),
                ("h", "3.2 On-board computer and storage"),
                (
                    "p",
                    "The on-board computer runs the flight software on a radiation-tolerant "
                    "microcontroller with 32 GB of flash storage for images, enough for about four days "
                    "of observations if ground contact is lost.",
                ),
            ],
            [
                ("h", "4. Operations phases"),
                (
                    "p",
                    "Launch and early orbit operations last three days and end once the solar wings are "
                    "deployed and two-way contact is stable. Commissioning takes six weeks, after which "
                    "routine operations are planned to continue for three years.",
                ),
                ("h", "5. End of life"),
                (
                    "p",
                    "At the end of the mission a deployable drag sail called the Furl, 1.5 square metres "
                    "in area, will be released to increase atmospheric drag. Re-entry is expected "
                    "within five years of deployment. Before the sail opens the spacecraft is passivated: "
                    "the battery is discharged and both transmitters are switched off.",
                ),
            ],
        ],
    ),
}


def pdf_escape(s: str) -> str:
    return s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")


def pdf_page_stream(blocks: list[tuple[str, str]]) -> bytes:
    lines = ["BT", "/F1 11 Tf", f"{PDF_LEADING} TL", f"72 {PDF_TOP} Td"]
    count = 0
    for kind, text in blocks:
        if kind == "h":
            lines.append(f"/F2 13 Tf ({pdf_escape(text)}) Tj T* /F1 11 Tf")
            count += 1
        else:
            for wrapped in textwrap.wrap(text, PDF_LINE_WIDTH, break_on_hyphens=False):
                lines.append(f"({pdf_escape(wrapped)}) Tj T*")
                count += 1
        lines.append("T*")
        count += 1
    lines.append("ET")
    if count > PDF_MAX_LINES:
        raise SystemExit(f"pdf page overflows: {count} lines")
    return ("\n".join(lines) + "\n").encode("ascii")


def build_pdf(title: str, pages: list[list[tuple[str, str]]]) -> bytes:
    n = len(pages)
    first_page = 5
    objects: list[bytes] = []
    kids = " ".join(f"{first_page + 2 * i} 0 R" for i in range(n))
    objects.append(b"<< /Type /Catalog /Pages 2 0 R >>")
    objects.append(f"<< /Type /Pages /Kids [{kids}] /Count {n} >>".encode("ascii"))
    objects.append(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>")
    objects.append(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>")
    for i, blocks in enumerate(pages):
        content_num = first_page + 2 * i + 1
        objects.append(
            (
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
                "/Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> "
                f"/Contents {content_num} 0 R >>"
            ).encode("ascii")
        )
        stream = pdf_page_stream(blocks)
        objects.append(b"<< /Length " + str(len(stream)).encode("ascii") + b" >>\nstream\n" + stream + b"endstream")
    info_num = len(objects) + 1
    objects.append(f"<< /Title ({pdf_escape(title)}) /Producer (gen_corpus.py) >>".encode("ascii"))

    out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = []
    for num, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += f"{num} 0 obj\n".encode("ascii") + body + b"\nendobj\n"
    xref = len(out)
    out += f"xref\n0 {len(objects) + 1}\n".encode("ascii")
    out += b"0000000000 65535 f \n"
    for off in offsets:
        out += f"{off:010d} 00000 n \n".encode("ascii")
    out += (
        f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R /Info {info_num} 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    ).encode("ascii")
    return bytes(out)


def build_pdfs() -> None:
    for rel, (title, pages) in PDFS.items():
        path = CORPUS / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(build_pdf(title, pages))
        PLAIN[rel] = "\n".join(text for page in pages for _, text in page)


# --------------------------------------------------------------------------
# DOCX (hand-built OOXML)
# --------------------------------------------------------------------------

W_NS = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"

CONTENT_TYPES = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
    '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
    '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
    '<Default Extension="xml" ContentType="application/xml"/>'
    '<Override PartName="/word/document.xml" '
    'ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
    '<Override PartName="/word/styles.xml" '
    'ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>'
    "</Types>"
)

ROOT_RELS = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
    '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
    '<Relationship Id="rId1" '
    'Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" '
    'Target="word/document.xml"/>'
    "</Relationships>"
)

DOC_RELS = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
    '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
    '<Relationship Id="rId1" '
    'Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" '
    'Target="styles.xml"/>'
    "</Relationships>"
)


def heading_style(style_id: str, name: str, level: int, size: int) -> str:
    return (
        f'<w:style w:type="paragraph" w:styleId="{style_id}">'
        f'<w:name w:val="{name}"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/>'
        f'<w:pPr><w:keepNext/><w:spacing w:before="240" w:after="120"/><w:outlineLvl w:val="{level}"/></w:pPr>'
        f'<w:rPr><w:b/><w:sz w:val="{size}"/></w:rPr></w:style>'
    )


STYLES = (
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
    f'<w:styles xmlns:w="{W_NS}">'
    '<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/>'
    '<w:pPr><w:spacing w:after="120"/></w:pPr><w:rPr><w:sz w:val="22"/></w:rPr></w:style>'
    + heading_style("Heading1", "heading 1", 0, 32)
    + heading_style("Heading2", "heading 2", 1, 26)
    + '<w:style w:type="table" w:default="1" w:styleId="TableNormal"><w:name w:val="Normal Table"/></w:style>'
    "</w:styles>"
)


def docx_para(text: str, style: str | None = None) -> str:
    ppr = f'<w:pPr><w:pStyle w:val="{style}"/></w:pPr>' if style else ""
    return f'<w:p>{ppr}<w:r><w:t xml:space="preserve">{xml_escape(text)}</w:t></w:r></w:p>'


def docx_table(header: list[str], rows: list[list[str]]) -> str:
    borders = "".join(
        f'<w:{side} w:val="single" w:sz="4" w:space="0" w:color="000000"/>'
        for side in ("top", "left", "bottom", "right", "insideH", "insideV")
    )
    out = [f'<w:tbl><w:tblPr><w:tblW w:w="0" w:type="auto"/><w:tblBorders>{borders}</w:tblBorders></w:tblPr>']
    out.append("<w:tblGrid>" + '<w:gridCol w:w="2400"/>' * len(header) + "</w:tblGrid>")
    for i, row in enumerate([header, *rows]):
        cells = []
        for cell in row:
            run_pr = "<w:rPr><w:b/></w:rPr>" if i == 0 else ""
            cells.append(
                f'<w:tc><w:tcPr><w:tcW w:w="2400" w:type="dxa"/></w:tcPr>'
                f'<w:p><w:r>{run_pr}<w:t xml:space="preserve">{xml_escape(cell)}</w:t></w:r></w:p></w:tc>'
            )
        out.append("<w:tr>" + "".join(cells) + "</w:tr>")
    out.append("</w:tbl>")
    return "".join(out)


DOCXS = {
    "utility/capital-plan.docx": [
        ("h1", "Varnholt Water Board Capital Plan 2026 to 2030"),
        ("h2", "Summary"),
        (
            "p",
            "The five-year plan commits 48.6 million crowns to renewing assets that are near the end of "
            "their life and to making the network more resilient to floods and drought. It was approved "
            "by the board at its December meeting and will be reviewed every year.",
        ),
        ("h2", "Programme of works"),
        (
            "p",
            "The table lists each project with its site, the years in which money is spent and its "
            "budget in millions of crowns.",
        ),
        (
            "table",
            (
                ["Project", "Site", "Years", "Budget (million crowns)"],
                [
                    ["Intake screen replacement", "Ostbank P-7", "2026 to 2027", "3.4"],
                    ["Flood barriers replaced by permanent walls", "Quayside", "2026 to 2028", "5.1"],
                    ["Trunk main renewal, Millrace to Canal Street", "Canal quarter", "2026", "2.8"],
                    ["Second settling lagoon", "Ostbank", "2027 to 2029", "9.7"],
                    ["Reservoir roof repair", "Tern Hill", "2027", "1.6"],
                    ["Lead service line replacement", "City-wide", "2026 to 2030", "12.0"],
                    ["Smart meter roll-out", "City-wide", "2026 to 2030", "8.3"],
                    ["Acoustic leak logger network", "Trunk mains", "2026 to 2028", "2.2"],
                    ["Standby generator", "Hollow Lane", "2028", "3.5"],
                ],
            ),
        ),
        ("h2", "Funding"),
        (
            "p",
            "About 60 per cent of the programme is paid for from tariffs, 30 per cent from a long-term "
            "loan from the municipal infrastructure fund and 10 per cent from a national resilience "
            "grant awarded for the Quayside walls and the second lagoon.",
        ),
        ("h2", "Risks"),
        (
            "p",
            "The largest risk is the second settling lagoon, where ground conditions are poorly known. A "
            "site investigation in spring 2026 will confirm whether piled foundations are needed; if "
            "they are, the cost of that project could rise by up to a fifth.",
        ),
    ],
    "game/club-handbook.docx": [
        ("h1", "Upland Tile Games Club Handbook"),
        ("h2", "Welcome"),
        (
            "p",
            "The club meets every Thursday from 19:00 in the upstairs room of the Pennant Hall. Most "
            "evenings feature Cairnfall, but members bring many other games and newcomers are always "
            "welcome.",
        ),
        ("h2", "Teaching new players"),
        (
            "p",
            "Each evening one experienced member acts as host for newcomers. Hosts teach with a "
            "three-turn demonstration played with open hands before starting a real game, and they "
            "remind learners about the weather phase at every turn until it becomes a habit.",
        ),
        ("h2", "Game library"),
        (
            "p",
            "Members may take home up to two games from the club cupboard for a fortnight. Loans are "
            "written in the blue ledger by the door. A game returned with missing pieces must be "
            "replaced or paid for at the shop price.",
        ),
        ("h2", "Membership and fees"),
        (
            "table",
            (
                ["Membership type", "Annual fee", "Includes"],
                [
                    ["Full", "30", "Voting, library, discounted Open entry"],
                    ["Student", "15", "Library, discounted Open entry"],
                    ["Family", "45", "Up to four people in one household"],
                    ["Visitor", "3 per evening", "Play only"],
                ],
            ),
        ),
        ("h2", "Running the qualifier"),
        (
            "p",
            "The club hosts a qualifying event for the Cairnfall Open each spring. Two members act as "
            "judges and the top two finishers earn invitations. The event follows the Open policy in "
            "full, so judges should read it again in the week before.",
        ),
    ],
}


def build_docx(blocks: list) -> tuple[bytes, str]:
    body = []
    plain = []
    for kind, value in blocks:
        if kind == "h1":
            body.append(docx_para(value, "Heading1"))
            plain.append(value)
        elif kind == "h2":
            body.append(docx_para(value, "Heading2"))
            plain.append(value)
        elif kind == "p":
            body.append(docx_para(value))
            plain.append(value)
        else:
            header, rows = value
            body.append(docx_table(header, rows))
            body.append(docx_para(""))
            plain.extend(" ".join(r) for r in [header, *rows])
    document = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
        f'<w:document xmlns:w="{W_NS}"><w:body>'
        + "".join(body)
        + '<w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr></w:body></w:document>'
    )
    parts = [
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", DOC_RELS),
        ("word/styles.xml", STYLES),
    ]
    import io

    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as zf:
        for name, data in parts:
            info = zipfile.ZipInfo(name, date_time=ZIP_EPOCH)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3
            info.external_attr = 0o644 << 16
            zf.writestr(info, data.encode("utf-8"))
    return buf.getvalue(), "\n".join(plain)


def build_docxs() -> None:
    for rel, blocks in DOCXS.items():
        data, plain = build_docx(blocks)
        path = CORPUS / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        PLAIN[rel] = plain


# --------------------------------------------------------------------------
# Email
# --------------------------------------------------------------------------

EMAILS = {
    "utility/flood-drill-summary.eml": (
        {
            "From": "Ilse Brandvold <ibrandvold@example.org>",
            "To": "Operations Team <operations@example.org>",
            "Cc": "Duty Managers <duty-managers@example.org>",
            "Subject": "Summary of the October flood drill at Quayside and Ostbank",
            "Date": "Thu, 16 Oct 2025 17:42:00 +0100",
            "Message-ID": "<20251016174200.flooddrill@example.org>",
        },
        [
            "Hi all,",
            "Thank you to everyone who took part in Tuesday's flood exercise. This is a short summary for "
            "those who could not attend; the full write-up will follow after the debrief.",
            "We simulated the river at Weir Bridge rising through the warning stage and on to the emergency "
            "stage over about three hours. As a reminder, the warning stage starts at 3.8 m and the "
            "emergency stage at 4.4 m.",
            "The main finding is that putting up the barriers took 47 minutes across both stations, against "
            "the 40 minute target. Most of the delay came from loading the posts at the Quayside depot, "
            "where the sections are stacked behind the portable pumps. We will re-arrange the store so the "
            "barrier kit comes out first.",
            "The switchover of the harbour district to Hollow Lane went smoothly. Pressure in the harbour "
            "stayed above 2.5 bar throughout, and no customer would have noticed anything.",
            "Two actions for now: crew leaders, please check that every member of your crew has a "
            "lifejacket that fits, and duty managers, please confirm your contact details on the roster by "
            "Friday.",
            "Best regards,\nIlse\n\nIlse Brandvold\nResilience Lead, Varnholt Water Board",
        ],
    ),
    "game/tournament-reminder.eml": (
        {
            "From": "Tomas Wrede <twrede@example.org>",
            "To": "Open Entrants <open-entrants@example.org>",
            "Subject": "Cairnfall Open this Saturday: check-in, clocks and tiebreaks",
            "Date": "Mon, 09 Jun 2025 09:15:00 +0200",
            "Message-ID": "<20250609091500.open-reminder@example.org>",
        },
        [
            "Hello everyone,",
            "The Open is this Saturday. Check-in opens at 09:00 and closes at 09:40 sharp, and the first "
            "round starts at 10:00. Anyone arriving after check-in closes will receive a bye for round one "
            "and join from round two.",
            "Please bring your own copy of the game and make sure the weather deck has all 24 cards. We "
            "have a few spare decks at the judges' table if yours is incomplete.",
            "A quick reminder about timing: each round is 75 minutes, and when time is called you play out "
            "the current round of turns so everyone has had the same number, then score as normal.",
            "If players end level on match points, we separate them by total game points, then by sigils "
            "claimed, then by the strength of the opponents they met.",
            "Lunch is after round three. See you on Saturday.",
            "Cheers,\nTomas\nHead judge, Cairnfall Open",
        ],
    ),
    "satellite/pass-schedule-update.eml": (
        {
            "From": "Anneli Sorvari <asorvari@example.org>",
            "To": "Mirelle Operations <mirelle-ops@example.org>",
            "Subject": "Mirelle-1 passes moved to Saltmarsh Point from 12 May",
            "Date": "Wed, 07 May 2025 14:05:00 +0000",
            "Message-ID": "<20250507140500.passes@example.org>",
        },
        [
            "Hi team,",
            "The antenna rotator gearbox at Hollin Ridge is being replaced from 12 to 16 May, so the dish "
            "will be parked and unusable for the whole week. All Mirelle-1 passes in that period move to "
            "the backup station at Saltmarsh Point.",
            "The shared university dish at Saltmarsh is only available to us for two passes a day, so "
            "expect roughly a third of the usual image volume. The imaging planners have already cut the "
            "lagoon list to match.",
            "The same rules apply at the backup site: we only schedule passes that peak more than 10 "
            "degrees above the horizon, and command uploads still need a pass above 25 degrees with two "
            "people on console.",
            "One gap remains. Pass HR-0412 on Tuesday morning has no equivalent from Saltmarsh, so the "
            "command plan for Wednesday has to go up on Monday evening instead.",
            "Thanks,\nAnneli\nGround segment lead",
        ],
    ),
}


def build_emails() -> None:
    for rel, (headers, paras) in EMAILS.items():
        lines = [f"{k}: {v}" for k, v in headers.items()]
        lines += ["MIME-Version: 1.0", "Content-Type: text/plain; charset=utf-8", "Content-Transfer-Encoding: 7bit"]
        body = []
        for para in paras:
            if "\n" in para:
                body.append(para)
            else:
                body.append("\n".join(textwrap.wrap(para, 72, break_on_hyphens=False)))
        text = "\n".join(lines) + "\n\n" + "\n\n".join(body) + "\n"
        write_text(rel, text, headers["Subject"] + "\n" + "\n".join(paras))


# --------------------------------------------------------------------------
# Notebooks
# --------------------------------------------------------------------------

NOTEBOOKS = {
    "utility/consumption-trends.ipynb": [
        (
            "markdown",
            """
            # Water consumption trends by district

            This notebook looks at metered consumption in the Varnholt supply area between 2021 and 2025,
            using quarterly billing reads joined to district boundaries.
        """,
        ),
        (
            "code",
            """
            import csv
            from collections import defaultdict

            def load_reads(path):
                with open(path, newline="") as fh:
                    return [row for row in csv.DictReader(fh) if row["estimated"] == "0"]

            reads = load_reads("quarterly_reads.csv")
        """,
        ),
        (
            "markdown",
            """
            ## Seasonal demand

            Demand peaks in July and August. Across the five years the summer peak averaged 1.6 times the
            winter low, and in the dry summer of 2022 it reached 1.9 times, which is when the Tern Hill
            reservoir came closest to its forty per cent operating floor.
        """,
        ),
        (
            "code",
            """
            by_quarter = defaultdict(float)
            for row in reads:
                by_quarter[row["quarter"]] += float(row["m3"])
            peak_ratio = max(by_quarter.values()) / min(by_quarter.values())
        """,
        ),
        (
            "markdown",
            """
            ## Per-capita consumption

            Dividing domestic volume by resident population gives litres per person per day. Cooper's Row
            is highest at 163 litres, largely because of its older houses with large gardens, while the
            harbour flats use the least at 112 litres.
        """,
        ),
        (
            "code",
            """
            by_district = defaultdict(float)
            for row in reads:
                if row["class"] == "domestic" and row["year"] == "2025":
                    by_district[row["district"]] += float(row["m3"])
            population = {"coopers_row": 9800, "harbour": 14200, "weir_bridge": 11050}
            litres_per_day = {
                d: by_district[d] * 1000 / population[d] / 365 for d in population
            }
        """,
        ),
        (
            "markdown",
            """
            ## Notes and caveats

            Estimated bills are excluded, which slightly under-counts districts with many unread meters.
            Population figures come from the 2023 municipal estimate and are held constant for all years.
        """,
        ),
    ],
    "satellite/power-analysis.ipynb": [
        (
            "markdown",
            """
            # Mirelle-1 orbit-average power analysis

            A quick check of whether the planned imaging schedule fits inside the power budget, using the
            mode powers from the power budget document and the worst-case eclipse.
        """,
        ),
        (
            "code",
            """
            PERIOD_MIN = 94.6
            ECLIPSE_MIN = 35.8
            SUNLIT_MIN = PERIOD_MIN - ECLIPSE_MIN
            ARRAY_W = 11.0       # nadir-pointing average while sunlit
            NOMINAL_W = 7.8
            IMAGING_W = 14.6
            BATTERY_WH = 38.0
        """,
        ),
        (
            "markdown",
            """
            ## Orbit-average power

            Averaged over a full orbit, generation in the imaging attitude comes to about 6.8 W once the
            eclipse is included, against 7.8 W for nominal operations alone, so every imaging session has
            to be paid for out of the battery and recharged in sun-pointing attitude afterwards.
        """,
        ),
        (
            "code",
            """
            def orbit_average_generation():
                return ARRAY_W * SUNLIT_MIN / PERIOD_MIN

            def session_energy_wh(minutes):
                return (IMAGING_W - NOMINAL_W) * minutes / 60
        """,
        ),
        (
            "markdown",
            """
            ## Battery depth of discharge

            The worst case is an eclipse spent in nominal mode immediately after imaging. One six-minute
            session leaves the cells at about 18 per cent discharged at the end of eclipse; two sessions
            reach 23 per cent; a third pushes the figure to 31 per cent, beyond the 25 per cent limit.
        """,
        ),
        (
            "code",
            """
            def depth_of_discharge(sessions, minutes=6):
                eclipse_wh = NOMINAL_W * ECLIPSE_MIN / 60
                return (eclipse_wh + sessions * session_energy_wh(minutes)) / BATTERY_WH

            [round(depth_of_discharge(n), 2) for n in (1, 2, 3)]
        """,
        ),
        (
            "markdown",
            """
            ## Findings

            At most two imaging sessions of six minutes fit into a single orbit. Downlink should not be
            scheduled in the same orbit as a second imaging session, because the transmitter's draw would
            take the battery past its limit before the next sunlit arc.
        """,
        ),
    ],
}


def nb_source(text: str) -> list[str]:
    lines = dedent(text).rstrip("\n").split("\n")
    return [line + "\n" for line in lines[:-1]] + [lines[-1]]


def build_notebooks() -> None:
    for rel, cells in NOTEBOOKS.items():
        out_cells = []
        plain = []
        for kind, text in cells:
            cell = {"cell_type": kind, "metadata": {}, "source": nb_source(text)}
            if kind == "code":
                cell["execution_count"] = None
                cell["outputs"] = []
            out_cells.append(cell)
            plain.append(dedent(text))
        nb = {
            "cells": out_cells,
            "metadata": {
                "kernelspec": {"display_name": "Python 3", "language": "python", "name": "python3"},
                "language_info": {"name": "python"},
            },
            "nbformat": 4,
            "nbformat_minor": 4,
        }
        write_text(rel, json.dumps(nb, indent=1, sort_keys=True, ensure_ascii=False) + "\n", "\n".join(plain))


# --------------------------------------------------------------------------
# Text and data files (plain text, logs, CSV, JSON, YAML, TOML)
# --------------------------------------------------------------------------


def telemetry_log() -> str:
    lines = []
    events = {
        37: "WARN adcs: star tracker lost lock during slew; falling back to sun sensors",
        81: "ERROR eps: battery string B undervoltage at 6.41 V; load shedding level 2 engaged",
        122: "INFO comms: S-band downlink resumed after Halvmark pass handover",
        167: "WARN thermal: imager focal plane at 31.8 C, above the 30 C imaging limit",
    }
    for i in range(200):
        minute, second = divmod(i * 17, 60)
        stamp = f"2025-03-14T06:{minute % 60:02d}:{second:02d}Z"
        body = events.get(
            i, f"DEBUG hk: housekeeping frame {i} nominal, bus 7.{i % 9}2 V, obc load {20 + i % 13} percent"
        )
        lines.append(f"{stamp} mirelle-1 {body}")
    return "\n".join(lines) + "\n"


def meter_csv() -> str:
    rows = ["meter_id,district,reading_m3,read_on,status"]
    districts = ["Harbour", "Ostbank", "Millfield", "Quayside", "Upper Varn"]
    for i in range(120):
        status = "ok"
        if i == 73:
            status = "suspected tamper: seal broken, flow reversed"
        elif i % 29 == 11:
            status = "estimated: no access"
        rows.append(
            f"VW-{4000 + i},{districts[i % 5]},{(i * 37) % 900 + 12}.{i % 10},2025-02-{1 + i % 27:02d},{status}"
        )
    return "\n".join(rows) + "\n"


def stations_yaml() -> str:
    out = ["# Mirelle-1 ground segment configuration", "ground_stations:"]
    stations = [
        ("halvmark", "Halvmark", 67.9, 5.0, "primary"),
        ("orrsund", "Orrsund", 59.3, 10.0, "backup"),
        ("tessel", "Tessel Point", 44.1, 7.5, "campaign"),
    ]
    for key, name, lat, elevation, role in stations:
        out += [
            f"  {key}:",
            f"    name: {name}",
            f"    latitude_deg: {lat}",
            f"    min_elevation_deg: {elevation}",
            f"    role: {role}",
            "    bands:",
            "      - s_band_downlink",
            "      - uhf_uplink",
        ]
    out += [
        "scheduling:",
        "  max_passes_per_day: 6",
        "  command_upload_deadline_min: 12",
        "  conflict_policy: prefer_primary_then_longest_pass",
    ]
    return "\n".join(out) + "\n"


def scoring_toml() -> str:
    return dedent(
        """
        # Cairnfall scoring constants used by the club's score app.

        [cairns]
        stone_points = 1
        completed_cairn_bonus = 4
        sigil_cairn_multiplier = 2

        [ridges]
        points_per_tile = 1
        unbroken_ridge_bonus = 3
        minimum_length = 3

        [weather]
        gale_discard_penalty = 2
        fog_hand_limit = 3

        [tiebreak]
        order = ["most_cairns", "longest_ridge", "fewest_discards"]
        """
    )


def pumps_json() -> str:
    pumps = {
        "Quayside": {"duty_pumps": 3, "standby_pumps": 1, "rated_flow_ls": 410, "start_level_m": 2.35},
        "Ostbank": {"duty_pumps": 2, "standby_pumps": 1, "rated_flow_ls": 260, "start_level_m": 1.9},
        "Millfield": {"duty_pumps": 2, "standby_pumps": 2, "rated_flow_ls": 180, "start_level_m": 3.1},
    }
    doc = {
        "network": "Varnholt Water Board raw water",
        "revision": "2025-01",
        "stations": {
            name: dict(cfg, alarm={"high_level_m": cfg["start_level_m"] + 1.2, "notify": "control-room"})
            for name, cfg in pumps.items()
        },
        "rotation": {"interval_hours": 168, "skip_if_alarm": True},
    }
    return json.dumps(doc, indent=2) + "\n"


def garden_txt() -> str:
    paragraphs = [
        "Fennick Road Community Garden: notes from the autumn work morning. These notes are kept as a plain "
        "text file in the shared folder so anyone can add to them without special software.",
        "The rainwater butts by the shed were drained and scrubbed. Two of them had cracked lids, which "
        "were replaced with the spare lids from the tool store. The hose reel was wound in for winter.",
        "Plot 14 has been empty since the summer. The committee agreed to offer it to the next person on "
        "the waiting list, and to split the overgrown blackberry bed between plots 13 and 15 if nobody takes it.",
        "Leaf mould: the wire cage behind the compost bays is now full. Leaves collected after this date go "
        "into the second cage, which was rebuilt with new chicken wire and four oak stakes.",
        "Next work morning is the first Saturday of December. Bring gloves; the tea urn is being repaired.",
    ]
    return "\n\n".join(textwrap.fill(p, width=72) for p in paragraphs) + "\n"


DATA_FILES = {
    "satellite/telemetry.log": telemetry_log,
    "utility/meter-readings.csv": meter_csv,
    "satellite/ground-stations.yaml": stations_yaml,
    "game/scoring.toml": scoring_toml,
    "utility/pump-stations.json": pumps_json,
    "misc/work-morning-notes.txt": garden_txt,
}


def build_data_files() -> None:
    for rel, make in DATA_FILES.items():
        write_text(rel, make())


# Tasks on the text and data files, numbered after every other task so earlier ids stay put. Each names
# the gold line (`meta.gold_line`): `citations.py` checks the cited line range holds it. Rows are
# (key, gold, variant, query, a substring of the gold line, rare term or None).
DATA_TASKS = [
    (
        "d_tracker",
        "satellite/telemetry.log",
        "question",
        "When did the spacecraft's star tracker lose lock, and what did attitude control fall back to?",
        "star tracker lost lock",
        None,
    ),
    (
        "d_undervoltage",
        "satellite/telemetry.log",
        "rare_term",
        "battery string B undervoltage load shedding",
        "undervoltage at 6.41 V",
        "undervoltage",
    ),
    (
        "d_tamper",
        "utility/meter-readings.csv",
        "question",
        "Which water meter reading was flagged as a suspected tamper?",
        "suspected tamper",
        None,
    ),
    (
        "d_orrsund",
        "satellite/ground-stations.yaml",
        "keywords",
        "Orrsund backup ground station minimum elevation",
        "min_elevation_deg: 10.0",
        None,
    ),
    (
        "d_ridge",
        "game/scoring.toml",
        "keywords",
        "unbroken ridge bonus points scoring constant",
        "unbroken_ridge_bonus",
        None,
    ),
    (
        "d_quayside_pumps",
        "utility/pump-stations.json",
        "question",
        "How many duty pumps does the Quayside raw water station run, and at what rated flow?",
        '"rated_flow_ls": 410',
        None,
    ),
    (
        "d_plot14",
        "misc/work-morning-notes.txt",
        "question",
        "What will happen to the empty plot 14 and the blackberry bed if nobody takes it?",
        "blackberry bed",
        None,
    ),
]


def gold_line(rel: str, marker: str) -> int:
    lines = PLAIN[rel].splitlines()
    found = [n for n, line in enumerate(lines, 1) if marker in line]
    if len(found) != 1:
        raise SystemExit(f"{rel}: marker {marker!r} on lines {found}")
    return found[0]


# --------------------------------------------------------------------------
# Tasks
# --------------------------------------------------------------------------

# Base tasks: (key, gold, variant, query, section, page, rare_term).
# rare_term, when set, must occur in exactly one document.
BASE_TASKS = [
    # Water utility
    (
        "u_trigger",
        "utility/flood-response.pdf",
        "question",
        "At what river height does the utility move into its emergency flood stage?",
        "2. Trigger levels",
        1,
        None,
    ),
    (
        "u_quayside",
        "utility/flood-response.pdf",
        "question",
        "How is the harbour district kept supplied if the Quayside station has to be shut down in a flood?",
        "4. Station protection",
        2,
        None,
    ),
    (
        "u_boil",
        "utility/flood-response.pdf",
        "question",
        "How many rounds of clean bacteria tests are needed before people can stop boiling their water?",
        "7. Recovery",
        3,
        None,
    ),
    ("u_gauge", "utility/flood-response.pdf", "rare_term", "E-114 gauge", "2. Trigger levels", 1, "E-114"),
    (
        "u_drill",
        "utility/flood-drill-summary.eml",
        "keywords",
        "flood exercise barrier delay depot posts",
        None,
        None,
        None,
    ),
    (
        "u_lagoon",
        "utility/pumping-stations.md",
        "question",
        "Where is muddy river water held before it can be treated?",
        "Raw water path",
        None,
        None,
    ),
    (
        "u_rotation",
        "utility/pumping-stations.md",
        "question",
        "On which day do the working and backup pumps change roles?",
        "Duty and standby rotation",
        None,
        None,
    ),
    (
        "u_loggers",
        "utility/pumping-stations.md",
        "keywords",
        "acoustic leak logger trial trunk main",
        "Telemetry and alarms",
        None,
        None,
    ),
    ("u_wb41", "utility/billing-policy.md", "rare_term", "WB-41 irrigation charge", "Tariff schedule", None, "WB-41"),
    (
        "u_leak",
        "utility/billing-policy.md",
        "question",
        "Will I get money back if a hidden pipe on my property was leaking and pushed up my bill?",
        "Leak allowances",
        None,
        None,
    ),
    (
        "u_hardship",
        "utility/billing-policy.md",
        "keywords",
        "spread bill monthly instalments unpaid",
        "Hardship and payment plans",
        None,
        None,
    ),
    (
        "u_taste",
        "utility/water-quality-report.md",
        "question",
        "Why did the drinking water have an odd earthy flavour after the hot weather?",
        "Taste and odour episode",
        None,
        None,
    ),
    (
        "u_geosmin",
        "utility/water-quality-report.md",
        "rare_term",
        "geosmin",
        "Taste and odour episode",
        None,
        "geosmin",
    ),
    (
        "u_lead",
        "utility/water-quality-report.md",
        "keywords",
        "lead pipe connections replaced this year",
        "Lead service lines",
        None,
        None,
    ),
    (
        "u_ortho",
        "utility/water-quality-report.md",
        "rare_term",
        "orthophosphate dosing",
        "Lead service lines",
        None,
        "orthophosphate",
    ),
    (
        "u_streets",
        "utility/outage-notice.html",
        "question",
        "Which roads are completely without water on the first day of the pipe works?",
        "Affected streets",
        None,
        None,
    ),
    (
        "u_vulnerable",
        "utility/outage-notice.html",
        "question",
        "Will someone bring drinking water to elderly residents if the supply is off for most of the day?",
        "Help for vulnerable customers",
        None,
        None,
    ),
    (
        "u_screens",
        "utility/capital-plan.docx",
        "question",
        "What will renewing the river inlet screens cost?",
        "Programme of works",
        None,
        None,
    ),
    (
        "u_funding",
        "utility/capital-plan.docx",
        "question",
        "How is the five-year investment programme paid for?",
        "Funding",
        None,
        None,
    ),
    (
        "u_percapita",
        "utility/consumption-trends.ipynb",
        "question",
        "Which neighbourhood uses the most water per resident each day?",
        "Per-capita consumption",
        None,
        None,
    ),
    (
        "u_seasonal",
        "utility/consumption-trends.ipynb",
        "keywords",
        "summer peak versus winter low demand ratio",
        "Seasonal demand",
        None,
        None,
    ),
    # Board game
    (
        "g_rain",
        "game/rules-overview.md",
        "question",
        "What can a wet weather card do to a tile and the stones on it?",
        "Turn sequence",
        None,
        None,
    ),
    (
        "g_stones",
        "game/rules-overview.md",
        "question",
        "How many stones does each person start with in Cairnfall?",
        "Components",
        None,
        None,
    ),
    ("g_end", "game/rules-overview.md", "question", "When does a game of Cairnfall finish?", "End of game", None, None),
    (
        "g_ties",
        "game/tournament-policy.md",
        "question",
        "If two players finish the Open level on match points, how is their order decided?",
        "Tiebreakers",
        None,
        None,
    ),
    (
        "g_slow",
        "game/tournament-policy.md",
        "keywords",
        "slow play repeat offence sanction",
        "Conduct and penalties",
        None,
        None,
    ),
    ("g_quarry", "game/scoring-reference.html", "rare_term", "Quarry rule", "The Quarry rule", None, "quarry"),
    (
        "g_three",
        "game/scoring-reference.html",
        "question",
        "How many points does a cairn three stones tall score?",
        "Scoring table",
        None,
        None,
    ),
    (
        "g_solo",
        "game/rulebook-appendix.pdf",
        "question",
        "Is there a way to play Cairnfall by yourself?",
        "A.2 Solo play against the Warden",
        1,
        None,
    ),
    (
        "g_sigil",
        "game/rulebook-appendix.pdf",
        "question",
        "Can a player hand a sigil they already took to someone else?",
        "Appendix B: Rulings and frequently asked questions",
        2,
        None,
    ),
    (
        "g_warden",
        "game/rulebook-appendix.pdf",
        "rare_term",
        "Warden automaton opponent",
        "A.2 Solo play against the Warden",
        1,
        "warden",
    ),
    (
        "g_library",
        "game/club-handbook.docx",
        "keywords",
        "borrowing club games loan period",
        "Game library",
        None,
        None,
    ),
    ("g_pennant", "game/club-handbook.docx", "rare_term", "Pennant Hall", "Welcome", None, "Pennant Hall"),
    (
        "g_checkin",
        "game/tournament-reminder.eml",
        "question",
        "What time does registration close on the morning of the Open?",
        None,
        None,
        None,
    ),
    # Satellite
    (
        "s_imaging",
        "satellite/power-budget.md",
        "question",
        "How many watts does the spacecraft draw while the camera is taking pictures?",
        "Operating modes",
        None,
        None,
    ),
    (
        "s_dod",
        "satellite/power-budget.md",
        "question",
        "How far may the battery be drained during routine operations?",
        "Battery",
        None,
        None,
    ),
    ("s_2s2p", "satellite/power-budget.md", "rare_term", "2S2P cells", "Battery", None, "2S2P"),
    (
        "s_htr",
        "satellite/thermal-control.html",
        "rare_term",
        "THERM-HTR-02 thresholds",
        "Heaters",
        None,
        "THERM-HTR-02",
    ),
    (
        "s_mli",
        "satellite/thermal-control.html",
        "question",
        "What insulation is wrapped around the battery and the camera optics?",
        "Passive control",
        None,
        None,
    ),
    (
        "s_limits",
        "satellite/thermal-control.html",
        "keywords",
        "survival temperature range reaction wheels",
        "Temperature limits",
        None,
        None,
    ),
    (
        "s_mask",
        "satellite/ground-station-passes.md",
        "question",
        "What is the lowest angle above the horizon at which a contact gets scheduled?",
        "Pass prediction",
        None,
        None,
    ),
    (
        "s_volume",
        "satellite/ground-station-passes.md",
        "keywords",
        "daily image data volume S-band link",
        "Downlink budget",
        None,
        None,
    ),
    (
        "s_twoperson",
        "satellite/ground-station-passes.md",
        "question",
        "Who has to check a command plan before it is sent to the spacecraft?",
        "Commanding windows",
        None,
        None,
    ),
    ("s_hr0412", "satellite/pass-schedule-update.eml", "rare_term", "HR-0412", None, None, "HR-0412"),
    (
        "s_backup",
        "satellite/pass-schedule-update.eml",
        "question",
        "Why are the satellite contacts going to the backup site for a week?",
        None,
        None,
        None,
    ),
    (
        "s_orbit",
        "satellite/mission-overview.pdf",
        "question",
        "What altitude and crossing time was chosen for the satellite's orbit?",
        "2. Orbit",
        1,
        None,
    ),
    (
        "s_eol",
        "satellite/mission-overview.pdf",
        "question",
        "How will the spacecraft be brought down once the mission is over?",
        "5. End of life",
        3,
        None,
    ),
    ("s_furl", "satellite/mission-overview.pdf", "rare_term", "Furl sail", "5. End of life", 3, "furl"),
    (
        "s_adcs",
        "satellite/mission-overview.pdf",
        "keywords",
        "reaction wheels magnetorquers pointing accuracy",
        "3.1 Attitude determination and control",
        2,
        None,
    ),
    (
        "s_sessions",
        "satellite/power-analysis.ipynb",
        "question",
        "How many imaging sessions per orbit can the power system support?",
        "Findings",
        None,
        None,
    ),
    # Singletons
    (
        "m_fridge",
        "misc/sourdough-starter.md",
        "question",
        "How frequently should a refrigerated starter be refreshed?",
        "Feeding schedule",
        None,
        None,
    ),
    (
        "m_trouble",
        "misc/sourdough-starter.md",
        "keywords",
        "grey liquid pink streaks throw away culture",
        "Signs of trouble",
        None,
        None,
    ),
    (
        "m_plots",
        "misc/community-garden.md",
        "keywords",
        "garden plot waiting list half plot",
        "Plot allocation",
        None,
        None,
    ),
    (
        "m_hoses",
        "misc/community-garden.md",
        "question",
        "When are hoses banned at the allotments in summer?",
        "Water use",
        None,
        None,
    ),
    (
        "m_sunday",
        "misc/library-hours.html",
        "question",
        "Is the library open at the weekend on Sundays?",
        "Opening hours",
        None,
        None,
    ),
    (
        "m_harrowgate",
        "misc/library-hours.html",
        "rare_term",
        "Harrowgate Lane opening times",
        "Opening hours",
        None,
        "Harrowgate",
    ),
]

# Base tasks duplicated with a mime_type filter set to the gold file's type.
FILTERED_KEYS = [
    # text/markdown (common)
    "u_lagoon",
    "u_rotation",
    "u_wb41",
    "u_leak",
    "g_stones",
    "g_ties",
    "s_imaging",
    "s_mask",
    # text/html (mid)
    "u_streets",
    "u_vulnerable",
    "g_quarry",
    "g_three",
    "s_htr",
    "m_sunday",
    # rare types
    "u_trigger",
    "u_boil",
    "g_sigil",
    "s_eol",
    "u_screens",
    "g_library",
    "u_drill",
    "g_checkin",
    "s_backup",
    "u_percapita",
    "s_sessions",
]

# Filtered tasks where the filter decides: the topic has a near-duplicate in
# another format that would plausibly rank first without the filter.
FILTER_DECIDES = [
    (
        "utility/flood-drill-summary.eml",
        "question",
        "How long did it take to put up the flood barriers at Quayside and Ostbank, and against what target?",
        None,
        None,
    ),
    ("game/tournament-reminder.eml", "keywords", "Cairnfall Open round length and tiebreak order", None, None),
    (
        "satellite/pass-schedule-update.eml",
        "keywords",
        "minimum pass elevation and upload rules at the backup ground station",
        None,
        None,
    ),
    (
        "satellite/power-analysis.ipynb",
        "keywords",
        "battery depth of discharge after imaging in eclipse",
        "Battery depth of discharge",
        None,
    ),
    (
        "utility/capital-plan.docx",
        "keywords",
        "Quayside flood barriers permanent walls budget",
        "Programme of works",
        None,
    ),
]


def mime_of(rel: str) -> str:
    return MIME_BY_EXT[Path(rel).suffix]


def selectivities() -> dict[str, str]:
    files = sorted(PLAIN)
    out = {}
    for mime in MIME_BY_EXT.values():
        n = sum(1 for f in files if mime_of(f) == mime)
        out[mime] = f"{n / len(files):.2f}"
    return out


def make_task(  # noqa: PLR0913, PLR0917
    gold: str,
    variant: str,
    query: str,
    section: str | None,
    page: int | None,
    mime: str | None = None,
    sel: str | None = None,
) -> dict:
    meta = {"variant": variant, "selectivity": sel if mime else "none"}
    if page is not None:
        meta["gold_page"] = page
    if section:
        meta["gold_section"] = section
    args = {"query": query}
    if mime:
        args["mime_type"] = mime
    return {"args": args, "gold": [gold], "k": TASK_K, "meta": meta, "mode": "docs", "scoring": "ranked"}


def data_tasks(lowered: dict[str, str]) -> list[dict]:
    tasks = []
    for key, gold, variant, query, marker, rare in DATA_TASKS:
        if rare is not None:
            hits = [rel for rel, text in lowered.items() if rare.lower() in text]
            if hits != [gold]:
                raise SystemExit(f"{key}: rare term {rare!r} found in {hits}")
        task = make_task(gold, variant, query, None, None)
        task["meta"]["gold_line"] = gold_line(gold, marker)
        tasks.append(task)
    return tasks


def build_tasks() -> list[dict]:
    sel = selectivities()
    by_key = {}
    tasks = []
    lowered = {rel: text.lower() for rel, text in PLAIN.items()}
    for key, gold, variant, query, section, page, rare in BASE_TASKS:
        if key in by_key:
            raise SystemExit(f"duplicate task key {key}")
        if gold not in PLAIN:
            raise SystemExit(f"{key}: gold {gold} not generated")
        if rare is not None:
            hits = [rel for rel, text in lowered.items() if rare.lower() in text]
            if hits != [gold]:
                raise SystemExit(f"{key}: rare term {rare!r} found in {hits}")
        if page is not None and not gold.endswith(".pdf"):
            raise SystemExit(f"{key}: gold_page on a non-PDF gold")
        by_key[key] = (gold, variant, query, section, page)
        tasks.append(make_task(gold, variant, query, section, page))
    for key in FILTERED_KEYS:
        gold, variant, query, section, page = by_key[key]
        mime = mime_of(gold)
        tasks.append(make_task(gold, variant, query, section, page, mime, sel[mime]))
    for gold, variant, query, section, page in FILTER_DECIDES:
        mime = mime_of(gold)
        tasks.append(make_task(gold, variant, query, section, page, mime, sel[mime]))
    tasks.extend(data_tasks(lowered))
    for i, task in enumerate(tasks, start=1):
        task["id"] = f"docs-corpus-{i:04d}"
    return tasks


# --------------------------------------------------------------------------
# Main
# --------------------------------------------------------------------------


def check_markdown_shape() -> None:
    import re

    for rel in MARKDOWN:
        text = (CORPUS / rel).read_text("utf-8")
        if not 1500 <= len(text) <= 4600:
            raise SystemExit(f"{rel}: {len(text)} characters")
    sections = re.split(r"^## ", (CORPUS / "game/rules-overview.md").read_text("utf-8"), flags=re.MULTILINE)
    longest = max(len(s) for s in sections)
    if longest <= 2500:
        raise SystemExit(f"no section over 2500 characters (longest {longest})")


def check_emails() -> None:
    for rel in EMAILS:
        msg = message_from_string((CORPUS / rel).read_text("utf-8"))
        for header in ("From", "To", "Subject", "Date", "Message-ID"):
            if not msg[header]:
                raise SystemExit(f"{rel}: missing {header}")


def main() -> None:
    if CORPUS.exists():
        shutil.rmtree(CORPUS)
    CORPUS.mkdir(parents=True)
    build_markdown()
    build_html()
    build_pdfs()
    build_docxs()
    build_emails()
    build_notebooks()
    build_data_files()
    check_markdown_shape()
    check_emails()
    tasks = build_tasks()
    with TASKS.open("w", encoding="utf-8", newline="\n") as fh:
        for task in tasks:
            fh.write(json.dumps(task, sort_keys=True, ensure_ascii=False) + "\n")
    print(f"wrote {len(PLAIN)} documents and {len(tasks)} tasks")


if __name__ == "__main__":
    main()
