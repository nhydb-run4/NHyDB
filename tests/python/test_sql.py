"""Differenztests NHySQL-0.24 gegen Python-v0.27 (Lexer, Parser, Compiler, Ausführung)."""
import importlib
import random

import pytest

import nhydb.query as rq
from ref_v027 import diff


@pytest.fixture(scope="module")
def pq(ref):
    return importlib.import_module("nhydb_query_v027")


def outcome(fn, *a, **k):
    try:
        r = fn(*a, **k)
    except Exception as e:  # noqa: BLE001
        return ("exc", type(e).__name__, str(e))
    return ("ok", repr(r))


CORPUS = [
    "SELECT id FROM records",
    "select ID , title AS t from main.records r where r.category = 1 and amount between 100 and 200 order by amount desc, id asc limit 3",
    "SELECT id AS nummer FROM records r WHERE r.category = ? AND r.active = ? ORDER BY r.id LIMIT 10 OFFSET 1",
    "SELECT category AS gruppe, COUNT(*) AS n, SUM(amount) summe, MIN(amount), max(r.amount), AVG(amount) FROM records r GROUP BY category ORDER BY gruppe",
    "SELECT COUNT(*) FROM records WHERE category = :cat",
    "SELECT id FROM records WHERE TOKEN(title, 'fox') OR TOKENS_ALL(title, 'red', 'fox') OR TOKENS_ANY(body, ?, 'x')",
    "SELECT id FROM records WHERE body CONTAINS 'ab''cd' AND ICONTAINS(body, 'ABCD') AND title ICONTAINS 'x'",
    "SELECT id, relevance FROM records WHERE FULLTEXT(body, 'quick fox') AND category = 2 ORDER BY relevance DESC",
    "SELECT id FROM records WHERE FULLTEXT_ANY(body, 'a b') AND PHRASE(body, 'quick brown') AND body PHRASE 'x y' AND body FULLTEXT 'z'",
    "SELECT id, tagline FROM records WHERE tagline IS NOT NULL AND title IS NULL AND EXISTS(body)",
    "SELECT * FROM main.records WHERE id = 1;",
    "SELECT r.* FROM records r",
    "EXPLAIN SELECT id FROM records WHERE category = 1",
    "EXPLAIN QUERY PLAN SELECT id FROM records",
    "SELECT id FROM records WHERE id = 1 OR id = 2 AND active = 0",
    "SELECT id FROM records WHERE NOT (category = 1) AND NOT NOT id <> 3",
    "SELECT id FROM records WHERE @u BETWEEN 1 AND 5 AND STRUCT.record_n = 'N3' AND @N >= 4 AND @layout_kind = 'CLASSIC'",
    "SELECT id FROM records WHERE x.id = 1",
    "SELECT id FROM records WHERE @zz = 1",
    "SELECT id FROM records WHERE STRUCT.foo = 1",
    "SELECT id FROM records WHERE a.b.c = 1",
    "SELECT id FROM records WHERE id = NULL",
    "SELECT id FROM records WHERE id == 1",
    "SELECT id FROM records WHERE TOKEN(title, 'a', 'b')",
    "SELECT id FROM records WHERE CONTAINS(title)",
    "SELECT DISTINCT id FROM records",
    "SELECT id FROM records GROUP BY id HAVING COUNT(*) > 1",
    "SELECT id FROM records LIMIT -1",
    "SELECT id FROM records LIMIT 1.5",
    "SELECT id FROM records LIMIT TRUE",
    "SELECT id FROM records OFFSET 'x'",
    "SELECT id FROM a.b.c",
    "SELECT FROM records",
    "SELECT id records",
    "SELECT id FROM records WHERE",
    "SELECT id FROM records; SELECT 1",
    "DELETE FROM records",
    "SELECT id FROM records WHERE title = 'unterminated",
    "SELECT \"weird \"\" name\", `back``tick` FROM records",
    "SELECT id FROM records /* unterminated",
    "SELECT id -- comment\nFROM records\n  WHERE\n    id = ?",
    "SELECT id FROM records WHERE id = :",
    "SELECT id FROM records WHERE @ = 1",
    "SELECT id FROM records WHERE id = 1e5 AND x = -2.5E-3 AND y = +7 AND z = 1. AND w = 1.e2",
    "SELECT id FROM records WHERE id = ٣ AND x = 1٣",
    "SELECT id FROM records WHERE id = ①",
    "SELECT id FROM records WHERE id = 1e",
    "SELECT id FROM records WHERE id = 1 # 2",
    "SELECT ä_ß, Größe FROM records WHERE straße = 'x' AND $x = 1",
    "SELECT id FROM records WHERE id = TRUE AND x = false AND y IS NOT NULL",
    "SELECT COUNT(id) AS c, COUNT FROM records",
    "SELECT id AS FROM records",
    "SELECT id FROM records AS",
    "SELECT id FROM records r WHERE id IN (1, 2)",
    "SELECT 5 FROM 7",
    "SELECT id FROM records ORDER BY",
    "SELECT id FROM records ORDER BY id DESC,",
    "\n\n   SELECT id\n   FROM records\n   WHERE id = = 2",
]

PARAMS = [None, [1, 2, 3], {"cat": 2}, "ab", (7,), {"x": 1}, 5]


@pytest.mark.parametrize("text", CORPUS)
def test_scan_and_parse_identical(pq, text):
    a = outcome(pq._sql_scan, text)
    b = outcome(rq._sql_scan, text)
    assert a == b, text
    for params in PARAMS:
        a = outcome(pq.parse_sql, text, params)
        b = outcome(rq.parse_sql, text, params)
        assert a == b, (text, params)


ATOMS = ["SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "(", ")", ",", ".", "*", "=", "<>", "<=", "BETWEEN", "IS", "NULL",
         "id", "title", "records", "r", "'x'", "'it''s'", "1", "-2", "3.5", "?", ":cat", "@u", "STRUCT", "COUNT", "SUM",
         "TOKEN", "FULLTEXT", "PHRASE", "CONTAINS", "AS", "ORDER", "BY", "DESC", "GROUP", "LIMIT", "OFFSET", ";", "\n",
         "--c\n", "/*k*/", "\"q\"", "EXPLAIN", "relevance", "٣", "ä"]


@pytest.mark.parametrize("seed", range(6))
def test_random_token_soup_identical(pq, seed):
    rnd = random.Random(seed)
    base = [x for x in CORPUS if x.startswith("SELECT") and "\n" not in x]
    for _ in range(400):
        if rnd.random() < 0.5:
            words = rnd.choice(base).split(" ")
            for _ in range(rnd.randint(1, 3)):
                op = rnd.random()
                pos = rnd.randrange(len(words) + 1)
                if op < 0.4 and words:
                    words.pop(min(pos, len(words) - 1))
                elif op < 0.8:
                    words.insert(pos, rnd.choice(ATOMS))
                elif words:
                    words[min(pos, len(words) - 1)] = rnd.choice(ATOMS)
            text = " ".join(words)
        else:
            text = " ".join(rnd.choice(ATOMS) for _ in range(rnd.randint(1, 18)))
        params = rnd.choice(PARAMS)
        assert outcome(pq._sql_scan, text) == outcome(rq._sql_scan, text), text
        assert outcome(pq.parse_sql, text, params) == outcome(rq.parse_sql, text, params), (text, params)


SETUP = r'''
from nhydb_v027 import NHyDB
from nhydb_query_v027 import QueryEngine
db = NHyDB.create("s.nhydb")
db.create_realm(1, "main", base_reserve_to=16)
db.create_shape(1, 1, "row", [
    {"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "title", "type": "utf8"},
    {"v": 3, "name": "body", "type": "utf8"}, {"v": 4, "name": "category", "type": "varuint"},
    {"v": 5, "name": "amount", "type": "varint"}, {"v": 6, "name": "active", "type": "u8"},
    {"v": 7, "name": "Score", "type": "f64"},
])
db.create_classic_layout(1, 1, "records", shape_id=1, base_reserve_to=16)
db.define_extension(1, 100, "utf8", name="tagline")
db.define_extension(1, 101, "varint", name="score")
db.create_index(1, 1, "idx_id", "id")
db.create_index(2, 1, "idx_category", "category")
db.create_index(3, 1, "idx_amount", "amount")
db.create_token_index(4, 1, "idx_title_token", "title")
db.create_trigram_index(5, 1, "idx_body_trigram", "body")
db.create_fulltext_index(6, 1, "idx_body_fulltext", "body")
db.create_fulltext_index(7, 1, "idx_tagline_fulltext", "tagline")
words = ["quick", "brown", "fox", "red", "blue", "bird", "abcd", "zz"]
rows = []
for u in range(1, 41):
    rows.append([u, f"{words[u % 8].title()} {words[(u * 3) % 8].title()}", " ".join(words[(u * k) % 8] for k in range(1, 5)),
                 u % 3, (u * 37) % 500 - 100, u % 2, (u % 7) / 3])
db.put_dense_batch(1, 1, rows, shape_id=1, extensions_by_u={u: {"tagline": f"tag {words[u % 8]}"} for u in range(1, 41, 5)})
db.build_indexes(commit=True)
db.create_realm(2, "second")
db.create_shape(2, 2, "row2", [{"v": 1, "name": "id", "type": "varuint"}])
db.create_classic_layout(2, 2, "records", shape_id=2)
db.create_column_layout(3, 2, "cols", fields=[{"u": 50, "name": "a", "type": "varint"}])
db.put_dense_batch(2, 1, [[u] for u in range(1, 6)], shape_id=2)
db.commit()
q = QueryEngine(db)
'''

STATEMENTS = [
    ("SELECT id, title FROM main.records WHERE category = 1 AND amount BETWEEN 100 AND 200 ORDER BY amount DESC, id ASC LIMIT 3", None),
    ("SELECT id AS nummer FROM main.records r WHERE r.category = ? AND r.active = ? ORDER BY r.id LIMIT 10 OFFSET 1", [1, 1]),
    ("SELECT category AS gruppe, COUNT(*) AS n, SUM(amount) AS summe, MIN(amount), MAX(amount), AVG(amount) FROM main.records GROUP BY category ORDER BY gruppe", None),
    ("SELECT COUNT(*) AS n FROM main.records WHERE category = 1", None),
    ("SELECT COUNT(*) FROM main.records WHERE amount > 0 AND TOKEN(title, 'fox')", None),
    ("SELECT COUNT(*), COUNT(tagline) FROM realm1.records", None),
    ("SELECT id FROM r1.records WHERE TOKEN(title, 'fox') ORDER BY id", None),
    ("SELECT id FROM realm_1.records WHERE TOKENS_ALL(title, 'red', 'fox') ORDER BY id DESC", None),
    ("SELECT id FROM 1.records WHERE body CONTAINS 'abcd' ORDER BY id", None),
    ("SELECT id, body FROM main.layout_1 WHERE ICONTAINS(body, 'ABCD') ORDER BY id LIMIT 4", None),
    ("SELECT id FROM main.layout1 WHERE FULLTEXT(body, 'quick fox') ORDER BY id", None),
    ("SELECT id FROM main.1 WHERE PHRASE(body, 'quick brown') ORDER BY id", None),
    ("SELECT id, relevance FROM main.records WHERE FULLTEXT(body, 'quick fox') ORDER BY relevance DESC LIMIT 3", None),
    ("SELECT id, relevance AS rel FROM main.records WHERE FULLTEXT(body, 'quick fox') AND category = 2 ORDER BY relevance DESC", None),
    ("SELECT title, relevance FROM main.records WHERE FULLTEXT_ANY(body, 'bird zz') AND amount > 0", None),
    ("SELECT id FROM main.records WHERE FULLTEXT(body, 'fox') ORDER BY relevance ASC", None),
    ("SELECT id FROM main.records WHERE FULLTEXT(body, 'fox') ORDER BY relevance DESC, id", None),
    ("SELECT COUNT(*), relevance FROM main.records WHERE FULLTEXT(body, 'fox')", None),
    ("SELECT id, relevance FROM main.records WHERE category = 1", None),
    ("SELECT id, tagline FROM main.records WHERE tagline IS NOT NULL ORDER BY id", None),
    ("SELECT id FROM main.records WHERE tagline IS NULL ORDER BY id LIMIT 5", None),
    ("SELECT * FROM main.records WHERE id = 1", None),
    ("SELECT r.*, id AS x FROM main.records r WHERE id < 3", None),
    ("SELECT q.* FROM main.records r", None),
    ("EXPLAIN SELECT id FROM main.records WHERE category = 1", None),
    ("EXPLAIN QUERY PLAN SELECT COUNT(*) FROM main.records WHERE FULLTEXT(body, 'fox') AND NOT category = 2", None),
    ("SELECT id FROM main.records WHERE category = :cat ORDER BY id", {"cat": 2}),
    ("SELECT id FROM main.records WHERE id = 1 OR id = 2 AND active = 0 ORDER BY id", None),
    ("SELECT id FROM main.records WHERE NOT (category = 1) ORDER BY id LIMIT 6 OFFSET 2", None),
    ("SELECT id FROM main.records WHERE @u BETWEEN 3 AND 9 AND STRUCT.layout_kind = 'CLASSIC' ORDER BY id", None),
    ("SELECT id FROM main.records WHERE @record_n = ?", ["N3"]),
    ("SELECT ID, TITLE FROM main.RECORDS WHERE CATEGORY = 2 ORDER BY Id DESC LIMIT 2", None),
    ("SELECT score FROM main.records", None),
    ("SELECT Score FROM main.records", None),
    ("SELECT nope FROM main.records", None),
    ("SELECT id FROM records", None),
    ("SELECT id FROM second.records", None),
    ("SELECT id FROM main.cols", None),
    ("SELECT id FROM second.cols", None),
    ("SELECT id FROM zzz.records", None),
    ("SELECT id FROM main.nolayout", None),
    ("SELECT id, id FROM main.records", None),
    ("SELECT id AS a, title AS A FROM main.records", None),
    ("SELECT id, COUNT(*) FROM main.records", None),
    ("SELECT id FROM main.records GROUP BY id", None),
    ("SELECT category, COUNT(*) AS n FROM main.records GROUP BY 1", None),
    ("SELECT category AS c, COUNT(*) AS n FROM main.records GROUP BY c ORDER BY n DESC, c", None),
    ("SELECT COUNT(*) AS n FROM main.records GROUP BY category ORDER BY n", None),
    ("SELECT SUM(title) FROM main.records", None),
    ("SELECT AVG(Score) AS a, MIN(title), MAX(tagline) FROM main.records WHERE amount < 50", None),
    ("SELECT id FROM main.records ORDER BY title, amount DESC LIMIT 7", None),
    ("SELECT id FROM main.records ORDER BY tagline DESC LIMIT 7", None),
    ("SELECT id FROM main.records WHERE amount >= ? AND amount <= ? ORDER BY amount", [0, 50]),
    ("SELECT id FROM main.records WHERE amount > ?", []),
    ("SELECT id FROM main.records WHERE amount > :x", [1]),
    ("SELECT id FROM main.records WHERE amount > ?", {"x": 1}),
    ("SELECT id FROM main.records WHERE r.amount > 1", None),
    ("SELECT id FROM main.records LIMIT 0", None),
    ("SELECT title FROM main.records WHERE title CONTAINS 'Fox' AND body TOKEN 'quick' ORDER BY title DESC", None),
    ("SELECT title FROM main.records WHERE title = 'Fox Red'", None),
    ("SELECT id FROM main.records WHERE id = 'x'", None),
    ("SELECT id FROM main.records WHERE Score > 1.0 ORDER BY Score DESC, id", None),
    ("SELECT * FROM second.records ORDER BY id DESC LIMIT 2", None),
    ("SELECT s.*, id AS k FROM second.records AS s WHERE s.id BETWEEN 2 AND 4", None),
    ("SELECT COUNT(*) AS n, SUM(id) AS s, AVG(id) FROM realm2.records WHERE NOT id = 3", None),
    ("SELECT id FROM second.layout_2 WHERE @v = 1 ORDER BY id", None),
]


def test_execute_sql_identical(tmp_path):
    lines = [SETUP]
    for i, (text, params) in enumerate(STATEMENTS):
        lines.append(f"attempt({i!r}, q.execute_sql, {text!r}, {params!r})")
        lines.append(f"attempt('c{i}', lambda: repr(q.compile_sql({text!r}, {params!r})))")
    lines.append("attempt('db.sql', db.sql, 'SELECT id FROM main.records WHERE category = :cat ORDER BY id', {'cat': 2})")
    lines.append("attempt('db.sql-lane', db.sql, 'SELECT COUNT(*) FROM main.records', lane='NR')")
    lines.append("attempt('db.compile', lambda: repr(db.compile_sql('SELECT id FROM main.records WHERE id = ?', [3], lane=1)))")
    py, rs = diff.compare("\n".join(lines), tmp_path)
    assert len(py) == len(rs)
    for a, b in zip(py, rs):
        assert a == b, (a, b)


def test_query_cli_sql(tmp_path):
    """`python -m nhydb.query` (CLI wie nhydb_query_v027) mit --sql und --params."""
    import json
    import subprocess
    import sys
    import nhydb
    p = tmp_path / "c.nhydb"
    db = nhydb.NHyDB.create(str(p))
    db.create_realm(1, "main")
    db.create_shape(1, 1, "row", [{"v": 1, "name": "id", "type": "varuint"}, {"v": 2, "name": "cat", "type": "varuint"}])
    db.create_classic_layout(1, 1, "records", shape_id=1)
    db.put_dense_batch(1, 1, [[u, u % 2] for u in range(1, 11)], shape_id=1)
    db.close()
    r = subprocess.run([sys.executable, "-m", "nhydb.query", str(p), "--sql", "SELECT id FROM records WHERE cat = ? ORDER BY id DESC LIMIT 2",
                        "--params", "[1]", "--json"], capture_output=True, text=True, check=True)
    out = json.loads(r.stdout)
    assert [x["id"] for x in out["rows"]] == [9, 7] and out["language"] == "NHySQL-0.24"
    r = subprocess.run([sys.executable, "-m", "nhydb.query", str(p), "--realm", "1", "--eq", "cat=0", "--return", "count"],
                       capture_output=True, text=True, check=True)
    assert "Count  : 5" in r.stdout
