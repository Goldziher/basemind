"""Process ownership in the hardening harness (scripts/hardening/common.py).

Windows keeps a dead parent's pid in its children and reuses pids, so a session's system processes
can name a CLI one-shot of the run as their parent. The windows-2022 soak counted winlogon.exe,
csrss.exe, dwm.exe and fontdrvhost.exe as children of the run for one sample and failed R5.
"""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "hardening"))

from common import expand_owned, parent_link_holds  # noqa: E402

ME = 1
CLI = 4000
GIT = 4100
CONHOST = 4200
WINLOGON = 600
CSRSS = 610

TABLE = {CLI: 3000, GIT: CLI, CONHOST: GIT, WINLOGON: CLI, CSRSS: CLI}
CREATED = {3000: 50, CLI: 100, GIT: 110, CONHOST: 120, WINLOGON: 5, CSRSS: 6}


def created(pid: int) -> int | None:
    return CREATED.get(pid)


class ParentLinkHoldsTest(unittest.TestCase):
    def test_holds_for_a_child_created_after_its_parent(self) -> None:
        self.assertTrue(parent_link_holds(GIT, CLI, created))

    def test_holds_for_a_child_created_in_the_same_tick(self) -> None:
        self.assertTrue(parent_link_holds(GIT, CLI, {GIT: 100, CLI: 100}.get))

    def test_fails_for_a_process_older_than_the_pid_it_names(self) -> None:
        self.assertFalse(parent_link_holds(WINLOGON, CLI, created))

    def test_fails_when_a_creation_time_is_unreadable(self) -> None:
        self.assertFalse(parent_link_holds(GIT, CLI, {GIT: 110}.get))
        self.assertFalse(parent_link_holds(GIT, CLI, {CLI: 100}.get))


class ExpandOwnedTest(unittest.TestCase):
    def test_keeps_real_descendants_and_drops_processes_naming_a_reused_pid(self) -> None:
        owned = expand_owned(TABLE, {CLI}, lambda pid, ppid: parent_link_holds(pid, ppid, created), ME)
        self.assertEqual(owned, {CLI, GIT, CONHOST})

    def test_follows_every_parent_link_when_links_always_hold(self) -> None:
        owned = expand_owned(TABLE, {CLI}, lambda _pid, _ppid: True, ME)
        self.assertEqual(owned, {CLI, GIT, CONHOST, WINLOGON, CSRSS})

    def test_never_claims_the_harness_itself(self) -> None:
        owned = expand_owned({ME: CLI, GIT: ME}, {CLI}, lambda _pid, _ppid: True, ME)
        self.assertEqual(owned, {CLI})


if __name__ == "__main__":
    unittest.main()
