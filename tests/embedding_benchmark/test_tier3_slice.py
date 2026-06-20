import sqlite3, sys, pathlib
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "scripts" / "embedding_benchmark"))
from build_tier3_slice import build


def _make_db(path):
    conn = sqlite3.connect(path)
    conn.executescript(
        """
        CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, language TEXT);
        CREATE TABLE nodes (id INTEGER PRIMARY KEY, file_id INTEGER, name TEXT,
                            qualified_name TEXT, type TEXT, is_test INTEGER);
        INSERT INTO files VALUES (1, 'a.rs', 'rust'), (2, 'b.ts', 'typescript');
        -- included: unique name, code type, len>=3, non-test
        INSERT INTO nodes VALUES (10, 1, 'unique_fn',  NULL, 'function', 0);
        INSERT INTO nodes VALUES (11, 2, 'GoodClass',  NULL, 'class',    0);
        -- excluded: duplicate name (ambiguous gold)
        INSERT INTO nodes VALUES (12, 1, 'dup_name',   NULL, 'function', 0);
        INSERT INTO nodes VALUES (13, 2, 'dup_name',   NULL, 'function', 0);
        -- excluded: test symbol
        INSERT INTO nodes VALUES (14, 1, 'test_thing', NULL, 'function', 1);
        -- excluded: non-code type
        INSERT INTO nodes VALUES (15, 1, 'SOME_MOD',   NULL, 'module',   0);
        -- excluded: name too short
        INSERT INTO nodes VALUES (16, 1, 'ab',         NULL, 'function', 0);
        """
    )
    conn.commit()
    conn.close()


def test_build_emits_only_unique_named_code_symbols(tmp_path):
    db = str(tmp_path / "index.db")
    _make_db(db)
    out = build([db], limit_per_db=250)
    by_query = {q["query"]: q for q in out}
    assert set(by_query) == {"unique_fn", "GoodClass"}
    assert by_query["unique_fn"]["gold_node_ids"] == [10]      # db_idx 0 -> gid == local
    assert by_query["unique_fn"]["query_class"] == "exact_symbol"
    assert by_query["unique_fn"]["source"] == "tier3"
    assert by_query["unique_fn"]["language"] == "rust"
    assert by_query["GoodClass"]["language"] == "typescript"


def test_build_namespaces_second_db(tmp_path):
    db0 = str(tmp_path / "a" / "index.db"); (tmp_path / "a").mkdir()
    db1 = str(tmp_path / "b" / "index.db"); (tmp_path / "b").mkdir()
    _make_db(db0); _make_db(db1)
    out = build([db0, db1], limit_per_db=250)
    golds = sorted(q["gold_node_ids"][0] for q in out)
    # db0: 10, 11 ; db1: 10_000_010, 10_000_011
    assert golds == [10, 11, 10_000_010, 10_000_011]


def test_limit_per_db_caps_output(tmp_path):
    db = str(tmp_path / "index.db")
    _make_db(db)
    out = build([db], limit_per_db=1)
    assert len(out) == 1


def test_unique_count_is_among_code_types_only(tmp_path):
    db = str(tmp_path / "index.db")
    conn = sqlite3.connect(db)
    conn.executescript(
        """
        CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, language TEXT);
        CREATE TABLE nodes (id INTEGER PRIMARY KEY, file_id INTEGER, name TEXT,
                            qualified_name TEXT, type TEXT, is_test INTEGER);
        INSERT INTO files VALUES (1, 'a.rs', 'rust');
        -- same name 'shared' as a function (code type) AND a module (non-code type)
        INSERT INTO nodes VALUES (20, 1, 'shared', NULL, 'function', 0);
        INSERT INTO nodes VALUES (21, 1, 'shared', NULL, 'module',   0);
        """
    )
    conn.commit()
    conn.close()
    out = build([db], limit_per_db=250)
    shared = [q for q in out if q["query"] == "shared"]
    assert len(shared) == 1                      # the function is emitted; module never counts
    assert shared[0]["gold_node_ids"] == [20]    # gold is the function node, not the module
