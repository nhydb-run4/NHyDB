"""Layout-independent query layer.

The expression objects (``Q``, ``FieldPredicate`` …) are lightweight, immutable
dataclasses with operator overloading (``&``, ``|``, ``~``, ``==``, ``<`` …).
Planning (``cardinality-cost-v1``), evaluation, providers, aggregation,
sorting and BM25 ranking run in the native ``QueryEngine``.

    user predicate -> provider(s) -> canonical candidate identities -> result
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Callable, Iterable, Mapping, Optional, Tuple

from ._native import (  # noqa: F401
    SQL_LANGUAGE_VERSION,
    parse_sql,
    _sql_scan,
    QueryEngine,
    QueryError,
    QueryPlanError,
    QueryDomainError,
    QueryLimitError,
    SQLSyntaxError,
    QUERY_VERSION,
    expr_from_spec,
)

DOMAIN_RECORD = "record"
DOMAIN_CELL = "cell"
_VALID_DOMAINS = {DOMAIN_RECORD, DOMAIN_CELL}


# ---------------------------------------------------------------------------
# Expression tree
# ---------------------------------------------------------------------------

class Expr:
    def __and__(self, other: "Expr") -> "Expr":
        return BoolExpr("and", (ensure_expr(self), ensure_expr(other)))

    def __or__(self, other: "Expr") -> "Expr":
        return BoolExpr("or", (ensure_expr(self), ensure_expr(other)))

    def __invert__(self) -> "Expr":
        return BoolExpr("not", (ensure_expr(self),))


@dataclass(frozen=True)
class BoolExpr(Expr):
    op: str
    children: Tuple[Expr, ...]


@dataclass(frozen=True)
class FieldPredicate(Expr):
    name: str
    op: str
    value: Any = None
    value2: Any = None
    options: Mapping[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class StructPredicate(Expr):
    name: str
    op: str
    value: Any = None
    value2: Any = None


@dataclass(frozen=True)
class MathPredicate(Expr):
    provider: str
    params: Mapping[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class DirectNPredicate(Expr):
    values: Tuple[Any, ...]
    domain: str = DOMAIN_CELL


@dataclass(frozen=True)
class AllPredicate(Expr):
    """The complete materialized universe of the target domain."""


class _Ref:
    predicate_type = FieldPredicate

    def __init__(self, name: str):
        self.name = str(name)

    def _p(self, op: str, value: Any = None, value2: Any = None,
           options: Optional[Mapping[str, Any]] = None) -> Expr:
        if self.predicate_type is FieldPredicate:
            return FieldPredicate(self.name, op, value, value2, dict(options or {}))
        return self.predicate_type(self.name, op, value, value2)

    def eq(self, value: Any) -> Expr:
        return self._p("eq", value)

    def ne(self, value: Any) -> Expr:
        return self._p("ne", value)

    def lt(self, value: Any) -> Expr:
        return self._p("lt", value)

    def le(self, value: Any) -> Expr:
        return self._p("le", value)

    def gt(self, value: Any) -> Expr:
        return self._p("gt", value)

    def ge(self, value: Any) -> Expr:
        return self._p("ge", value)

    def between(self, lo: Any, hi: Any) -> Expr:
        return self._p("between", lo, hi)

    def contains(self, value: Any, *, ngram: int = 3) -> Expr:
        return self._p("contains", value, options={"ngram": int(ngram), "casefold": False})

    def icontains(self, value: Any, *, ngram: int = 3) -> Expr:
        return self._p("icontains", value, options={"ngram": int(ngram), "casefold": True})

    def token(self, value: Any, *, casefold: bool = True, tokenizer: str = "simple") -> Expr:
        return self._p("token", value, options={"casefold": bool(casefold), "tokenizer": tokenizer})

    def tokens_all(self, *values: Any, casefold: bool = True, tokenizer: str = "simple") -> Expr:
        return self._p("tokens_all", tuple(values), options={"casefold": bool(casefold), "tokenizer": tokenizer})

    def tokens_any(self, *values: Any, casefold: bool = True, tokenizer: str = "simple") -> Expr:
        return self._p("tokens_any", tuple(values), options={"casefold": bool(casefold), "tokenizer": tokenizer})

    def fulltext(self, value: Any, *, mode: str = "all", casefold: bool = True,
                 tokenizer: str = "simple") -> Expr:
        mode = str(mode).lower()
        if mode not in {"all", "any"}:
            raise ValueError("FULLTEXT mode must be 'all' or 'any'")
        return self._p("fulltext", value, options={
            "mode": mode, "casefold": bool(casefold), "tokenizer": tokenizer, "positions": True
        })

    def phrase(self, value: Any, *, casefold: bool = True, tokenizer: str = "simple") -> Expr:
        return self._p("phrase", value, options={
            "casefold": bool(casefold), "tokenizer": tokenizer, "positions": True
        })

    def exists(self) -> Expr:
        return self._p("exists")

    def __eq__(self, other: Any) -> Expr:  # type: ignore[override]
        return self.eq(other)

    def __ne__(self, other: Any) -> Expr:  # type: ignore[override]
        return self.ne(other)

    def __lt__(self, other: Any) -> Expr:
        return self.lt(other)

    def __le__(self, other: Any) -> Expr:
        return self.le(other)

    def __gt__(self, other: Any) -> Expr:
        return self.gt(other)

    def __ge__(self, other: Any) -> Expr:
        return self.ge(other)

    __hash__ = None  # type: ignore[assignment]


class FieldRef(_Ref):
    predicate_type = FieldPredicate


class StructRef(_Ref):
    predicate_type = StructPredicate


class QueryBuilder:
    def field(self, name: str) -> FieldRef:
        return FieldRef(name)

    def struct(self, name: str) -> StructRef:
        return StructRef(name)

    @property
    def realm(self) -> StructRef:
        return StructRef("realm")

    @property
    def u(self) -> StructRef:
        return StructRef("u")

    @property
    def v(self) -> StructRef:
        return StructRef("v")

    @property
    def lane(self) -> StructRef:
        return StructRef("lane")

    @property
    def N(self) -> StructRef:
        return StructRef("N")

    @property
    def record_N(self) -> StructRef:
        return StructRef("record_N")

    def n(self, *values: Any, domain: str = DOMAIN_CELL) -> DirectNPredicate:
        domain = str(domain).lower()
        if domain not in _VALID_DOMAINS:
            raise QueryDomainError(f"domain must be record or cell, got: {domain!r}")
        return DirectNPredicate(tuple(values), domain)

    def math(self, provider: str, **params: Any) -> MathPredicate:
        return MathPredicate(str(provider), dict(params))

    def fulltext(self, field_name: str, value: Any, *, mode: str = "all",
                 casefold: bool = True, tokenizer: str = "simple") -> Expr:
        return self.field(field_name).fulltext(value, mode=mode, casefold=casefold, tokenizer=tokenizer)

    def phrase(self, field_name: str, value: Any, *, casefold: bool = True,
               tokenizer: str = "simple") -> Expr:
        return self.field(field_name).phrase(value, casefold=casefold, tokenizer=tokenizer)

    def all(self) -> AllPredicate:
        return AllPredicate()


Q = QueryBuilder()


def ensure_expr(value: Any) -> Expr:
    if not isinstance(value, Expr):
        raise TypeError(f"query expression expected, got: {type(value).__name__}")
    return value


# ---------------------------------------------------------------------------
# Context and result objects (for runtime providers)
# ---------------------------------------------------------------------------

@dataclass(frozen=True)
class CandidateSet:
    domain: str
    ns: Tuple[int, ...]
    source: str = ""

    def __post_init__(self) -> None:
        if self.domain not in _VALID_DOMAINS:
            raise QueryDomainError(f"invalid candidate domain {self.domain!r}")

    @property
    def count(self) -> int:
        return len(self.ns)

    @property
    def refs(self) -> Tuple[str, ...]:
        return tuple(f"N{x}" for x in self.ns)


@dataclass(frozen=True)
class QueryContext:
    """Context passed to runtime providers: ``realm`` (or None), ``lane``."""
    realm: Optional[int] = None
    lane: int = 0


@dataclass(frozen=True)
class MathProvider:
    name: str
    function: Callable[..., Iterable[Any]]
    domain: str = DOMAIN_CELL
    description: str = ""
    estimator: Optional[Callable[..., Optional[int]]] = None


# ---------------------------------------------------------------------------
# NHySQL (lexer/parser/compiler/execution are native; only the result objects live here)
# ---------------------------------------------------------------------------

@dataclass(frozen=True)
class SQLToken:
    kind: str
    value: Any
    pos: int


@dataclass(frozen=True)
class SQLSelectItem:
    kind: str                 # star | field | aggregate | relevance
    name: str = ""
    op: str = ""
    alias: Optional[str] = None


@dataclass(frozen=True)
class SQLOrderItem:
    name: str
    direction: str = "asc"


@dataclass(frozen=True)
class SQLSelectStatement:
    source: Tuple[str, ...]
    source_alias: Optional[str]
    select_items: Tuple[SQLSelectItem, ...]
    where: Expr
    group_by: Tuple[str, ...] = ()
    order_by: Tuple[SQLOrderItem, ...] = ()
    limit: Optional[int] = None
    offset: int = 0
    explain: bool = False
    text: str = ""


@dataclass(frozen=True)
class SQLCompiledQuery:
    statement: SQLSelectStatement
    layout_id: int
    layout_name: str
    realm: int
    lane: int
    expr: Expr
    return_mode: str
    select: Tuple[str, ...] = ()
    aggregate: Mapping[str, Any] = field(default_factory=dict)
    group_by: Tuple[str, ...] = ()
    order_by: Tuple[Tuple[str, str], ...] = ()
    limit: Optional[int] = None
    offset: int = 0
    ranked: bool = False
    output: Tuple[SQLSelectItem, ...] = ()
    all_fields: Tuple[str, ...] = ()


def _statement_from_raw(raw: Mapping[str, Any]) -> SQLSelectStatement:
    return SQLSelectStatement(
        source=tuple(raw["source"]), source_alias=raw["source_alias"],
        select_items=tuple(SQLSelectItem(**x) for x in raw["select_items"]),
        where=raw["where"], group_by=tuple(raw["group_by"]),
        order_by=tuple(SQLOrderItem(n, d) for n, d in raw["order_by"]),
        limit=raw["limit"], offset=raw["offset"], explain=raw["explain"], text=raw["text"],
    )


def _compiled_from_raw(raw: Mapping[str, Any]) -> SQLCompiledQuery:
    d = dict(raw)
    d["statement"] = _statement_from_raw(d["statement"])
    d["output"] = tuple(SQLSelectItem(**x) for x in d["output"])
    return SQLCompiledQuery(**d)


# ---------------------------------------------------------------------------
# Small CLI: filter flags, JSON specification or NHySQL.
# ---------------------------------------------------------------------------

def _scalar(text: str) -> Any:
    import json
    try:
        return json.loads(text)
    except Exception:
        return text


def _flag_expr(args: Any) -> Optional[Expr]:
    exprs = []
    for raw in args.eq or []:
        name, value = raw.split("=", 1)
        exprs.append(Q.field(name).eq(_scalar(value)))
    for raw in args.between or []:
        name, value = raw.split("=", 1); lo, hi = value.split(":", 1)
        exprs.append(Q.field(name).between(_scalar(lo), _scalar(hi)))
    for raw in args.token or []:
        name, value = raw.split("=", 1)
        exprs.append(Q.field(name).token(value))
    for raw in args.contains or []:
        name, value = raw.split("=", 1)
        exprs.append(Q.field(name).contains(value))
    for raw in args.icontains or []:
        name, value = raw.split("=", 1)
        exprs.append(Q.field(name).icontains(value))
    for raw in args.fulltext or []:
        name, value = raw.split("=", 1)
        exprs.append(Q.field(name).fulltext(value))
    for raw in args.phrase or []:
        name, value = raw.split("=", 1)
        exprs.append(Q.field(name).phrase(value))
    if args.u is not None:
        exprs.append(Q.u.eq(args.u))
    if args.v is not None:
        exprs.append(Q.v.eq(args.v))
    if not exprs:
        return None
    out = exprs[0]
    for x in exprs[1:]:
        out = out & x
    return out


def _print_human(result: Mapping[str, Any]) -> None:
    import json
    if result.get("language"):
        print(f"NHyDB {result.get('language')}")
        print(f"Statement: {result.get('statement')}")
        source = result.get("source") or {}
        if source:
            print(f"Source   : R{source.get('realm')} / {source.get('layout_name')} (layout {source.get('layout_id')})")
        if result.get("columns") is not None:
            print("Columns  :", ", ".join(str(x) for x in result.get("columns", ())))
        print(f"Returned : {result.get('returned')}")
        if result.get("matched") is not None:
            print(f"Matched  : {result.get('matched')}")
    else:
        print(f"NHyDB QUERY V{QUERY_VERSION}")
        print(f"Domain : {result.get('domain')}")
        print(f"Return : {result.get('return')}")
        print(f"Count  : {result.get('count')}")
    if "plan" in result:
        print("Plan   :")
        print(json.dumps(result["plan"], ensure_ascii=False, indent=2))
    rows = result.get("rows")
    if rows is not None:
        print("Rows   :")
        for row in rows:
            print("  " + json.dumps(row, ensure_ascii=False, default=str))
    elif "refs" in result:
        print("Refs   :", result.get("refs"))


def main(argv: Optional[Iterable[str]] = None) -> None:
    import argparse
    import json
    from pathlib import Path
    from . import NHyDB

    ap = argparse.ArgumentParser(description="NHyDB layout-independent query layer")
    ap.add_argument("file")
    ap.add_argument("--realm", type=int)
    ap.add_argument("--lane", default="NL")
    ap.add_argument("--eq", action="append", help="field=value; repeatable; AND semantics")
    ap.add_argument("--between", action="append", help="field=lo:hi; repeatable; AND semantics")
    ap.add_argument("--token", action="append", help="field=word; TOKEN predicate; repeatable")
    ap.add_argument("--contains", action="append", help="field=substring; case-sensitive; TRIGRAM if available")
    ap.add_argument("--icontains", action="append", help="field=substring; casefolded; compatible TRIGRAM if available")
    ap.add_argument("--fulltext", action="append", help="field=query; FULLTEXT all-terms predicate")
    ap.add_argument("--phrase", action="append", help="field=phrase; positional FULLTEXT phrase predicate")
    ap.add_argument("--u", type=int, help="structural anchor/cell u equality")
    ap.add_argument("--v", type=int, help="structural anchor/cell v equality")
    ap.add_argument("--spec", help="JSON query spec")
    ap.add_argument("--spec-file", help="path to JSON query spec")
    ap.add_argument("--sql", help="NHySQL SELECT statement")
    ap.add_argument("--sql-file", help="path to a UTF-8 file containing one NHySQL SELECT")
    ap.add_argument("--params", help="JSON array/object for ? or :name SQL parameters")
    ap.add_argument("--return", dest="return_mode", choices=["record", "cell", "n", "count", "fields", "aggregate", "ranked"], default="record")
    ap.add_argument("--select", action="append")
    ap.add_argument("--aggregate", help="JSON mapping, e.g. '{\"rows\":{\"op\":\"count\",\"field\":\"*\"}}'")
    ap.add_argument("--group-by", action="append")
    ap.add_argument("--order-by", action="append", help="field or -field; repeatable")
    ap.add_argument("--limit", type=int)
    ap.add_argument("--offset", type=int, default=0)
    ap.add_argument("--explain", action="store_true")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args(None if argv is None else list(argv))

    selected_inputs = sum(bool(x) for x in (args.spec, args.spec_file, args.sql, args.sql_file))
    if selected_inputs > 1:
        raise SystemExit("use only one input kind: --sql/--sql-file/--spec/--spec-file")
    spec = None
    sql_text = None
    if args.spec_file:
        spec = json.loads(Path(args.spec_file).read_text(encoding="utf-8"))
    elif args.spec:
        spec = json.loads(args.spec)
    elif args.sql_file:
        sql_text = Path(args.sql_file).read_text(encoding="utf-8")
    elif args.sql:
        sql_text = args.sql
    sql_params = json.loads(args.params) if args.params else None

    # N3 (RUN8): a query only reads; it must not take the write lock
    with NHyDB(args.file, readonly=True) as db:
        engine = QueryEngine(db)
        if sql_text is not None:
            if args.explain and not sql_text.lstrip().upper().startswith("EXPLAIN"):
                sql_text = "EXPLAIN " + sql_text
            result = engine.execute_sql(sql_text, parameters=sql_params, lane=args.lane)
        elif spec is not None:
            if "where" in spec:
                merged = dict(spec)
                if args.explain:
                    merged["explain"] = True
                result = engine.execute_spec(merged)
            else:
                expr = expr_from_spec(spec)
                result = engine.execute(expr, realm=args.realm, lane=args.lane,
                                        return_mode=args.return_mode, select=args.select,
                                        explain=args.explain)
        else:
            expr = _flag_expr(args)
            aggregate = json.loads(args.aggregate) if args.aggregate else None
            if expr is None:
                if aggregate:
                    expr = Q.all()
                else:
                    raise SystemExit("query needs --sql/--sql-file/--spec/--spec-file or at least one filter")
            result = engine.execute(expr, realm=args.realm, lane=args.lane,
                                    return_mode=args.return_mode, select=args.select,
                                    aggregate=aggregate, group_by=args.group_by,
                                    order_by=args.order_by, limit=args.limit, offset=args.offset,
                                    explain=args.explain)
    if args.json:
        print(json.dumps(result, ensure_ascii=False, indent=2, default=str))
    else:
        _print_human(result)


__all__ = [
    "QUERY_VERSION", "DOMAIN_RECORD", "DOMAIN_CELL",
    "QueryError", "QueryPlanError", "QueryDomainError", "QueryLimitError", "SQLSyntaxError",
    "SQL_LANGUAGE_VERSION", "SQLToken", "SQLSelectItem", "SQLOrderItem", "SQLSelectStatement", "SQLCompiledQuery",
    "parse_sql",
    "Expr", "BoolExpr", "FieldPredicate", "StructPredicate", "MathPredicate", "DirectNPredicate", "AllPredicate",
    "FieldRef", "StructRef", "QueryBuilder", "Q", "ensure_expr",
    "CandidateSet", "QueryContext", "MathProvider", "QueryEngine", "expr_from_spec",
]


if __name__ == "__main__":
    main()
