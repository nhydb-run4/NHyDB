"""A small document store with extensions, full-text search and NHySQL.

Articles are CLASSIC records; optional metadata lives in extensions. A
FULLTEXT index ranks search results with BM25, a VALUE index answers filters.

Run: python document_store.py               (needs: pip install nhydb)
"""
import os
import tempfile

import nhydb
from nhydb.query import Q, QueryEngine

ARTICLES = [
    ("Getting started", "install the package and create your first database", "guide", 2024),
    ("Backups", "copy the database file, the layout file and the key file while the database is closed", "ops", 2024),
    ("Indexes", "build indexes after writing data; a stale index is rebuilt with build_indexes", "guide", 2023),
    ("Full text", "the fulltext index ranks documents with bm25 and supports phrase search", "search", 2025),
    ("Key file", "without the key file the database cannot be opened; keep a copy of the key file", "ops", 2025),
]


def main():
    path = os.path.join(tempfile.mkdtemp(), "kb.nhydb")
    with nhydb.create(path) as db:
        db.create_realm(1, "kb", base_reserve_to=10)
        db.create_shape(1, 1, "article", [
            {"v": 1, "name": "title", "type": "utf8"},
            {"v": 2, "name": "body", "type": "utf8"},
            {"v": 3, "name": "topic", "type": "utf8"},
            {"v": 4, "name": "year", "type": "u16"},
        ])
        db.create_classic_layout(1, 1, "articles", shape_id=1)
        db.define_extension(1, 20, "utf8", name="author")
        for u, row in enumerate(ARTICLES, start=1):
            db.put_record(1, u, dict(zip(("title", "body", "topic", "year"), row)))
        db.put_extensions(1, 2, {"author": "ops team"})
        db.create_index(1, 1, "by_topic", "topic")
        db.create_fulltext_index(2, 1, "ft_body", "body")
        db.build_indexes(commit=True)

        hits = QueryEngine(db).search(Q.fulltext("body", "key file"), realm=1, select=["title"], limit=3)
        print("search 'key file':", [h["title"] for h in hits["rows"]])
        assert hits["rows"][0]["title"] == "Key file"

        r = db.sql("SELECT title, year FROM articles WHERE topic = :t ORDER BY year DESC", {"t": "ops"})
        print("ops articles:", [(x["title"], x["year"]) for x in r["rows"]])

        r = db.sql("SELECT topic, COUNT(*) AS n FROM articles GROUP BY topic ORDER BY n DESC, topic")
        print("per topic:", r["rows"])

        phrase = db.query(Q.phrase("body", "phrase search"), realm=1, return_mode="fields", select=["title"])
        print("phrase:", [x["title"] for x in phrase["rows"]])
        print("with author:", db.get(1, 2)["author"])
        assert db.verify(deep=True)["ok"]


if __name__ == "__main__":
    main()
