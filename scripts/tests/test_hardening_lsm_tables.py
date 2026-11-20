"""Which open files the hardening soak counts as live Fjall tables, and how it names the rest
(scripts/hardening/common.py).

R3 judges the daemon's descriptors net of its live tables, so a descriptor still held on a table a
compaction deleted must not count as live. R3t names the descriptor kinds that moved over load.
"""

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "hardening"))

import analyze  # noqa: E402
from common import fd_kind, is_live_lsm_table  # noqa: E402


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


class FdKindTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.db = Path(self.tmp.name) / "cache" / "groups" / "g" / "index.fjall"
        self.tables = self.db / "keyspaces" / "3" / "tables"
        self.tables.mkdir(parents=True)
        (self.tables / "7").write_bytes(b"table")

    def test_a_live_table_is_named_by_its_database(self) -> None:
        self.assertEqual(fd_kind(str(self.tables / "7")), "index.fjall tables")

    def test_a_journal_is_named_by_its_database(self) -> None:
        self.assertEqual(fd_kind(str(self.db / "0.jnl")), "index.fjall")

    def test_a_table_linux_marks_deleted_is_named_deleted(self) -> None:
        self.assertEqual(fd_kind(f"{self.tables / '7'} (deleted)"), "index.fjall (deleted)")

    def test_a_lance_data_file_is_named_by_its_dataset(self) -> None:
        data = Path(self.tmp.name) / "memory.lance" / "v1" / "memory.lance" / "data"
        self.assertEqual(fd_kind(str(data / "0a1b.lance")), "memory.lance")
        self.assertEqual(fd_kind(str(data.parent / "_versions" / "3.manifest")), "memory.lance")

    def test_linux_sockets_pipes_and_anon_inodes_are_named_by_type(self) -> None:
        self.assertEqual(fd_kind("socket:[48213]"), "socket")
        self.assertEqual(fd_kind("pipe:[48214]"), "pipe")
        self.assertEqual(fd_kind("anon_inode:[eventpoll]"), "anon_inode")

    def test_any_other_file_is_other(self) -> None:
        self.assertEqual(fd_kind("/dev/null"), "other")
        self.assertEqual(fd_kind("/tmp/gone (deleted)"), "other (deleted)")


class FdKindMovesTest(unittest.TestCase):
    def test_the_kinds_that_moved_are_listed_largest_first(self) -> None:
        series = [
            {"fd_kinds": {"index.fjall tables": 20, "socket": 4, "memory.lance": 2}},
            {"fd_kinds": {"index.fjall tables": 23, "socket": 4, "index.fjall (deleted)": 9}},
        ]
        self.assertEqual(
            analyze._fd_kind_moves(series),
            "descriptors by kind over load: index.fjall (deleted) +9, index.fjall tables +3, memory.lance -2",
        )

    def test_an_unmoved_census_says_so(self) -> None:
        series = [{"fd_kinds": {"socket": 4}}, {"fd_kinds": {"socket": 4}}]
        self.assertEqual(analyze._fd_kind_moves(series), "descriptors by kind over load: unchanged")

    def test_a_soak_without_the_census_says_so(self) -> None:
        self.assertEqual(analyze._fd_kind_moves([{"fds": 50}, {"fds": 51}]), "no descriptor kinds recorded")


if __name__ == "__main__":
    unittest.main()
