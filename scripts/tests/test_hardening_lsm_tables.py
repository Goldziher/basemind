"""Which open files the hardening soak counts as live Fjall tables (scripts/hardening/common.py).

R3 judges the daemon's descriptors net of its live tables, so a descriptor still held on a table a
compaction deleted must not count as live.
"""

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "hardening"))

from common import is_live_lsm_table  # noqa: E402


class IsLiveLsmTableTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.tables = Path(self.tmp.name) / "index.fjall" / "keyspaces" / "9" / "tables"
        self.tables.mkdir(parents=True)
        (self.tables / "12").write_bytes(b"table")
        (Path(self.tmp.name) / "index.fjall" / "0.jnl").write_bytes(b"journal")

    def test_a_table_on_disk_is_live(self) -> None:
        self.assertTrue(is_live_lsm_table(str(self.tables / "12")))

    def test_a_deleted_table_is_not_live(self) -> None:
        self.assertFalse(is_live_lsm_table(str(self.tables / "10")))

    def test_a_table_linux_marks_deleted_is_not_live(self) -> None:
        self.assertFalse(is_live_lsm_table(f"{self.tables / '12'} (deleted)"))

    def test_a_journal_is_not_a_table(self) -> None:
        self.assertFalse(is_live_lsm_table(str(Path(self.tmp.name) / "index.fjall" / "0.jnl")))

    def test_a_tables_dir_outside_a_keyspace_is_not_a_table(self) -> None:
        other = Path(self.tmp.name) / "tables"
        other.mkdir()
        (other / "1").write_bytes(b"x")
        self.assertFalse(is_live_lsm_table(str(other / "1")))


if __name__ == "__main__":
    unittest.main()
