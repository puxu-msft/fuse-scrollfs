import sqlite3, tempfile, unittest
from pathlib import Path
from harness import db


class TestDb(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name) / "harness.db"

    def tearDown(self):
        # 先关连接再删目录：否则 SQLite 连接随 GC 关闭会触发
        # ResourceWarning，掩盖真正的资源泄漏信号
        conn = getattr(self, "conn", None)
        if conn is not None:
            conn.close()
        self.tmp.cleanup()

    def test_connect_enables_wal_and_foreign_keys(self):
        conn = db.connect(self.path)
        self.addCleanup(conn.close)
        self.assertEqual(conn.execute("PRAGMA journal_mode").fetchone()[0], "wal")
        self.assertEqual(conn.execute("PRAGMA foreign_keys").fetchone()[0], 1)

    def test_migrate_is_idempotent(self):
        conn = db.connect(self.path)
        self.addCleanup(conn.close)
        db.migrate(conn)
        db.migrate(conn)
        tables = {r[0] for r in conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table'")}
        self.assertIn("operations", tables)
        self.assertIn("rounds", tables)
        self.assertIn("budget_days", tables)
        self.assertIn("proposals", tables)

    def test_operations_natural_key_is_unique_per_kind(self):
        conn = db.connect(self.path)
        self.addCleanup(conn.close)
        db.migrate(conn)
        conn.execute(
            "INSERT INTO operations(operation_id, round_id, kind, natural_key,"
            " payload_hash, phase, created_at, updated_at)"
            " VALUES('op1','r1','create_issue','nk1','h1','prepared',0,0)")
        with self.assertRaises(sqlite3.IntegrityError):
            conn.execute(
                "INSERT INTO operations(operation_id, round_id, kind, natural_key,"
                " payload_hash, phase, created_at, updated_at)"
                " VALUES('op2','r1','create_issue','nk1','h1','prepared',0,0)")


if __name__ == "__main__":
    unittest.main()
