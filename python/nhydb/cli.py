"""Kommandozeile der nativen NHyDB (``python -m nhydb``).

Reine Schnittstellenschicht: Argumente parsen und Ergebnisse formatieren.
Alle Datenbankoperationen laufen im nativen Kern.
"""
from __future__ import annotations

import argparse
import json
from typing import Any, Dict, List, Optional, Sequence, Tuple

from ._native import ENGINE_VERSION, NHyDB, cell_address, n_to_dynadron_address, normalize_lane

DEFAULT_BATCH_RECORDS = 10_000
_UNSET = object()

def _pjson(obj):
    print(json.dumps(obj, ensure_ascii=False, indent=2, sort_keys=True))
def _compact_int(n: int) -> str:
    n = int(n)
    if n <= 0:
        return str(n)
    t = n; exp = 0
    while t % 10 == 0:
        t //= 10; exp += 1
    if t == 1 and exp >= 6:
        return f"10^{exp}"
    text = str(n)
    if len(text) > 28:
        return f"{text[:10]}...{text[-8:]} ({len(text)} digits)"
    return text
def _human_layout(obj: Any) -> str:
    layouts = obj if isinstance(obj, list) else ([] if obj is None else [obj])
    if not layouts:
        return "No matching Layout."
    chunks = []
    for d in layouts:
        lines = [f"LAYOUT {d['layout_id']} - {d['name']}",
                 f"Realm       : {d['realm_id']}",
                 f"Kind        : {d['kind']}",
                 f"u range     : {d['u_start']} .. {d['u_end'] if d.get('u_end') is not None else 'unlimited'}",
                 f"Priority    : {d.get('priority', 0)}"]
        if d.get('description'):
            lines.append(f"Description : {d['description']}")
        sm = d.get('spacemap') or {}
        lines += ["", "SpaceMap"]
        if sm:
            w = max(len(str(k)) for k in sm)
            for k, v in sm.items():
                lines.append(f"  {str(k):<{w}} : {v}")
        else:
            lines.append("  (none)")
        lanes = d.get('lane_semantics') or {}
        if lanes:
            lines += ["", "Lanes"]
            for k, v in lanes.items():
                lines.append(f"  {k:<9} : {v}")
        if d.get('kind') == 'CLASSIC':
            lines += ["", "BASE"]
            base = d.get('base') or []
            if base:
                for f in base:
                    enum = f.get('enum')
                    extra = "" if not enum else " [" + ", ".join(f"{k}={v}" for k, v in enum.items()) + "]"
                    lines.append(f"  v{f['v']:<8} {f['name']:<20} {f['type']}{extra}")
            else:
                lines.append("  (none)")
            r = d.get('base_reserve') or {}
            lines += ["", "Reserved"]
            if r.get('to'):
                lines.append(f"  v{r.get('from', 1)} .. v{_compact_int(r['to'])}")
            else:
                lines.append("  (none)")
            lines += ["", "Extensions"]
            exts = d.get('extensions') or []
            if exts:
                for e in exts:
                    lines.append(f"  v{_compact_int(e['v']):<18} {e.get('name') or '-':<22} {e['type']}")
            else:
                lines.append("  (none)")
        params = d.get('params') or {}
        if d.get('kind') != 'CLASSIC' and params:
            lines += ["", "Parameters"]
            for k, v in params.items():
                if k == 'fields':
                    continue
                lines.append(f"  {k:<10} : {_compact_int(v) if isinstance(v, int) else v}")
            fields = params.get('fields') or []
            if fields:
                lines += ["", "Fields"]
                for f in fields:
                    pos = f"u{f['u']}" if 'u' in f else f"offset {f.get('offset', 0)}"
                    lines.append(f"  {pos:<14} {f.get('name','field'):<20} {f.get('type','bytes')}")
        chunks.append("\n".join(lines))
    return "\n\n".join(chunks)
def _human_describe(d: Dict[str, Any]) -> str:
    lines = [f"NHyDB DESCRIBE {d.get('N_ref', 'N'+str(d.get('N','?')))}", "",
             "Address",
             f"  Realm      : {d.get('realm')}",
             f"  u          : {d.get('u')}",
             f"  v          : {_compact_int(d.get('v_local', d.get('v', 0)))}",
             f"  lane       : {d.get('lane_name', d.get('lane'))}",
             f"  occupied   : {'yes' if d.get('occupied') else 'no'}"]
    lay = d.get('layout')
    if lay:
        lines += ["", "Layout",
                  f"  id         : {lay.get('layout_id')}",
                  f"  name       : {lay.get('name')}",
                  f"  kind       : {lay.get('kind')}"]
    interp = d.get('interpretation') or {}
    if interp:
        lines += ["", "Interpretation"]
        for k, v in interp.items():
            lines.append(f"  {k:<10} : {v}")
    return "\n".join(lines)
def _human_health(d: Dict[str, Any]) -> str:
    lines = ["NHyDB HEALTH", "",
             f"File        : {d.get('file')}",
             f"Engine      : {d.get('engine')}",
             f"Format      : {d.get('format')}",
             f"Mode        : {'deep' if d.get('deep') else 'standard'}", ""]
    for c in d.get('checks', []):
        mark = {'ok':'OK', 'warning':'WARN', 'error':'ERROR'}.get(c.get('status'), str(c.get('status')).upper())
        lines.append(f"{c.get('name', ''):<14}: {mark}")
        details = c.get('details') or {}
        if details.get('errors'):
            for e in details['errors'][:5]: lines.append(f"  - {e}")
        if details.get('stale'):
            for e in details['stale'][:5]: lines.append(f"  - stale: {e}")
    lines += ["", f"Result      : {d.get('status','').upper()}", f"Time        : {d.get('seconds',0):.3f} s"]
    return "\n".join(lines)
def _human_stats(d: Dict[str, Any]) -> str:
    st = d.get('storage') or {}
    lines = ["NHyDB STATS", "",
             f"File                 : {d.get('file')}",
             f"Engine / format      : {d.get('engine')} / {d.get('format')}",
             f"File size            : {d.get('file_size')} ({d.get('file_bytes',0):,} bytes)",
             f"Realms               : {len(d.get('realms') or [])}",
             f"Layouts              : {len(d.get('layouts') or [])}",
             f"Shapes               : {len(d.get('shapes') or [])}",
             f"Extension definitions: {len(d.get('extensions') or [])}",
             f"Indexes              : {len(d.get('indexes') or [])}", "",
             "Materialization",
             f"  Dense records       : {st.get('physical_dense_records',0):,}",
             f"  Sparse u groups     : {st.get('sparse_u_groups',0):,}",
             f"  Sparse cells        : {st.get('materialized_sparse_cells',0):,}",
             f"  Sparse bytes        : {st.get('sparse_bytes',0):,}"]
    bpc = st.get('sparse_bytes_per_cell')
    lines.append(f"  Bytes / sparse cell : {'-' if bpc is None else f'{bpc:.3f}'}")
    lines += ["", "Physical blocks"]
    for name, x in (st.get('by_block_type') or {}).items():
        lines.append(f"  {name:<18} {x['blocks']:>6} blocks  {x['bytes']:>12,} bytes")
    return "\n".join(lines)
def _human_route(d: Dict[str, Any]) -> str:
    lines=["NHyDB ROUTE","",
           f"Realm       : {d.get('realm')}", f"u           : {d.get('u')}",
           f"v           : {'-' if d.get('v') is None else _compact_int(d.get('v'))}",
           f"lane        : {d.get('lane_name')}",
           f"winner      : {d.get('layout_id')} - {d.get('layout_name')} ({d.get('kind')})" if d.get('layout_id') else "winner      : none",
           f"candidates  : {d.get('candidate_count',0)}"]
    if d.get('candidate_count',0)>1:
        lines += ["", "Routing candidates"]
        for x in d.get('candidates',[]):
            lines.append(f"  {x['layout_id']}: {x['name']} | {x['kind']} | priority={x['priority']} | u={x['u_start']}..{x['u_end'] if x['u_end'] is not None else 'unlimited'}")
    return "\n".join(lines)
def _human_layout_diagnostics(d: Dict[str, Any]) -> str:
    s=d.get('summary',{})
    lines=["NHyDB LAYOUT CHECK","",f"Realms      : {s.get('realms',0)}",f"Overlaps    : {s.get('overlaps',0)}",f"Conflicts   : {s.get('conflicts',0)}",f"Gaps        : {s.get('gaps',0)}"]
    for r in d.get('realms',[]):
        lines += ["", f"Realm {r['realm']} - {r['layout_count']} Layout(s)"]
        if r.get('overlaps'):
            lines.append("  Overlaps")
            for x in r['overlaps']:
                end='unlimited' if x['to'] is None else x['to']
                lines.append(f"    u{x['from']}..{end}: {x['layouts']} -> winner {x['winner']} [{x['resolution']}]")
        if r.get('gaps'):
            lines.append("  Gaps (informational)")
            for x in r['gaps'][:20]: lines.append(f"    u{x['from']}..{x['to']}")
        if r.get('missing_runtime_plugins'):
            lines.append("  Runtime plugins missing")
            for x in r['missing_runtime_plugins']: lines.append(f"    Layout {x['layout_id']}: {x['plugin']}")
        for risk in r.get('risks',[]): lines.append(f"  Risk: {risk}")
    return "\n".join(lines)
def _human_evolution_plan(d: Dict[str, Any]) -> str:
    imp = d.get("occupied_impact", {})
    lines = ["NHyDB LAYOUT EVOLUTION PLAN", "",
             f"Layout      : {d.get('layout_id')} - {d.get('layout_name')}",
             f"Realm       : {d.get('realm')}",
             f"Kind        : {d.get('kind')}",
             f"Result      : {str(d.get('status','?')).upper()}"]
    cur = d.get("current")
    if cur:
        lines += ["", "Current",
                  f"  u range   : {cur.get('u_start')} .. {cur.get('u_end') if cur.get('u_end') is not None else 'unlimited'}",
                  f"  priority  : {cur.get('priority')}"]
    prop = d.get("proposed", {})
    lines += ["", "Proposed",
              f"  u range   : {prop.get('u_start')} .. {prop.get('u_end') if prop.get('u_end') is not None else 'unlimited'}",
              f"  priority  : {prop.get('priority')}", "", "Routing changes"]
    changes = d.get("routing_changes", [])
    if not changes:
        lines.append("  (none)")
    for x in changes[:40]:
        end = 'unlimited' if x.get('to') is None else x.get('to')
        lines.append(f"  u{x.get('from')}..{end}: {x.get('old_layout_id')} -> {x.get('new_layout_id')}")
    lines += ["", "Occupied impact",
              f"  dense records <= : {imp.get('dense_records_at_risk_upper_bound',0)}",
              f"  sparse u groups  : {imp.get('sparse_u_groups_at_risk',0)}",
              f"  sparse cells     : {imp.get('sparse_cells_at_risk',0)}",
              f"  sample u         : {imp.get('sample_u',[])}"]
    return "\n".join(lines)
def _human_address(d: Dict[str, Any], title: str = "NHyDB ADDRESS") -> str:
    return "\n".join([title,"",
        f"N          : {d.get('N_ref','N'+str(d.get('N','?')))}",
        f"Realm      : {d.get('realm')}", f"u          : {d.get('u')}",
        f"v          : {_compact_int(d.get('v_local',d.get('v',0)))}",
        f"lane       : {d.get('lane_name',d.get('lane'))}",
        f"G          : {d.get('G')}"])
def _human_get_result(d: Dict[str, Any]) -> str:
    kind = d.get("result")
    if kind == "unresolved":
        return "\n".join([
            "NHyDB GET", "",
            f"Result      : UNRESOLVED ({d.get('reason')})",
            f"N           : {d.get('N')}",
            f"Address     : R{d.get('realm')},u{d.get('u')},v{d.get('v')},{d.get('lane_name')}",
        ])
    if kind == "cell":
        return "\n".join([
            "NHyDB GET CELL", "",
            f"Mode        : {d.get('requested_mode')} -> {d.get('effective_mode')}",
            f"N           : {d.get('N')}",
            f"Address     : R{d.get('realm')},u{d.get('u')},v{_compact_int(d.get('v',0))},{d.get('lane_name')}",
            f"Layout      : {d.get('layout_id')} {d.get('layout_kind') or ''} {d.get('layout_name') or ''}".rstrip(),
            f"Object      : {d.get('object')}",
            f"Record N    : {d.get('record_N')}",
            f"Field       : {d.get('field')}",
            f"Anchor      : {d.get('anchor')}",
            f"Present     : {d.get('present')}",
            f"Value       : {d.get('value')!r}",
        ])
    if kind == "record":
        lines = [
            "NHyDB GET RECORD", "",
            f"Mode        : {d.get('requested_mode')} -> {d.get('effective_mode')}",
            f"Request N   : {d.get('request_N', d.get('record_N'))}",
            f"Record N    : {d.get('record_N')}",
            f"Anchor      : R{d.get('anchor_address',{}).get('realm')},u{d.get('anchor_address',{}).get('u')},v{_compact_int(d.get('anchor_address',{}).get('v',0))},{d.get('lane_name')}",
            f"Layout      : {d.get('layout_id')} {d.get('layout_kind')} {d.get('layout_name')}",
            f"Object      : {d.get('object')}",
            f"Present     : {d.get('present')}",
            f"Anchor data : {d.get('anchor_present')}",
            f"Cells       : {d.get('cell_count',0)}", "",
            "Data",
        ]
        if not d.get("cells"):
            lines.append("  (no occupied cells)")
        else:
            for c in d.get("cells", []):
                mark = "*" if c.get("anchor") else " "
                lines.append(
                    f" {mark} {c.get('field'):<18} = {c.get('value')!r}  "
                    f"[{c.get('N')} | R{c.get('realm')},u{c.get('u')},v{_compact_int(c.get('v',0))},{c.get('lane_name')}]"
                )
        return "\n".join(lines)
    return json.dumps(d, ensure_ascii=False, indent=2)
def _parse_structural_get_target(text: str, *, default_realm: Optional[int] = None) -> Tuple[int, int, int, int]:
    """Parse R1,u2,v3[,lane] or u2,v3[,lane] with an explicit default Realm."""
    raw = str(text).strip().replace(" ", "")
    parts = raw.split(",")
    if parts and parts[0].lower().startswith("r"):
        if len(parts) not in {3, 4} or not parts[1].lower().startswith("u") or not parts[2].lower().startswith("v"):
            raise ValueError("Strukturadresse erwartet R<realm>,u<u>,v<v>[,NL|NR|0|1]")
        realm = int(parts[0][1:]); u = int(parts[1][1:]); v = int(parts[2][1:])
        lane_raw = "NL" if len(parts) == 3 else parts[3]
    else:
        if default_realm is None:
            raise ValueError("u,v-Kurzform braucht --realm")
        if len(parts) not in {2, 3} or not parts[0].lower().startswith("u") or not parts[1].lower().startswith("v"):
            raise ValueError("Kurzadresse erwartet u<u>,v<v>[,NL|NR|0|1] plus --realm")
        realm = int(default_realm); u = int(parts[0][1:]); v = int(parts[1][1:])
        lane_raw = "NL" if len(parts) == 2 else parts[2]
    if str(lane_raw).lower().startswith("s"):
        lane_raw = str(lane_raw)[1:]
    return realm, u, v, normalize_lane(lane_raw)
def _parse_scalar(text: str):
    try:
        return json.loads(text)
    except Exception:
        return text
def _parse_filters(args):
    f = {}
    for x in args.eq or []:
        name, value = x.split("=", 1)
        f[name] = _parse_scalar(value)
    for x in args.between or []:
        name, rng = x.split("=", 1)
        lo, hi = rng.split(":", 1)
        f[name] = (_parse_scalar(lo), _parse_scalar(hi))
    return f
def _parse_u_end_cli(value: Optional[str]):
    if value is None:
        return _UNSET
    if str(value).strip().lower() in {"none", "null", "unlimited", "inf", "infinite"}:
        return None
    return int(value)
def _parse_json_fields(items: Optional[Sequence[str]]) -> Optional[List[Dict[str, Any]]]:
    if not items:
        return None
    out = []
    for raw in items:
        obj = json.loads(raw)
        if not isinstance(obj, dict):
            raise ValueError("--add-field erwartet ein JSON-Objekt")
        out.append(obj)
    return out
def _evolution_kwargs(args) -> Dict[str, Any]:
    kw = {
        "name": args.name,
        "description": args.description,
        "u_start": args.u_start,
        "u_end": _parse_u_end_cli(args.u_end),
        "priority": args.priority,
        "add_fields": _parse_json_fields(args.add_field),
        "spacemap_update": None if args.spacemap_update is None else json.loads(args.spacemap_update),
    }
    if kw["u_end"] is _UNSET:
        del kw["u_end"]
    return kw
def main():
    p = argparse.ArgumentParser(description=f"NHyDB {ENGINE_VERSION} -- command line interface")
    sub = p.add_subparsers(dest="cmd", required=True)
    x = sub.add_parser("to-n", help="convert (Realm,u,v,lane) to a canonical N reference")
    x.add_argument("--realm", type=int, required=True)
    x.add_argument("--u", type=int, required=True)
    x.add_argument("--v", type=int, required=True)
    x.add_argument("--lane", default="NL", help="0/NL or 1/NR")
    x.add_argument("--json", action="store_true", help="machine-readable JSON output")
    x = sub.add_parser("from-n", help="convert N... to (Realm,u,v,lane)")
    x.add_argument("n")
    x.add_argument("--json", action="store_true", help="machine-readable JSON output")
    x = sub.add_parser("layout", aliases=["get-layout"], help="GET LAYOUT: show persisted Layout configuration")
    x.add_argument("file")
    x.add_argument("--realm", type=int)
    x.add_argument("--u", type=int)
    x.add_argument("--layout-id", type=int)
    x.add_argument("--json", action="store_true", help="machine-readable JSON output")
    x = sub.add_parser("layout-check", help="diagnose mixed-Realm Layout routing, gaps and overlaps")
    x.add_argument("file")
    x.add_argument("--realm", type=int)
    x.add_argument("--json", action="store_true", help="machine-readable JSON output")
    for cmd, help_text in (
        ("layout-plan", "dry-run a Layout evolution and show routing/ownership impact"),
        ("evolve-layout", "apply a safe Layout evolution; unsafe owner changes are rejected"),
    ):
        x = sub.add_parser(cmd, help=help_text)
        x.add_argument("file")
        x.add_argument("--layout-id", type=int, required=True)
        x.add_argument("--name")
        x.add_argument("--description")
        x.add_argument("--u-start", type=int)
        x.add_argument("--u-end", help="integer or unlimited/none")
        x.add_argument("--priority", type=int)
        x.add_argument("--add-field", action="append", help="JSON object; repeatable")
        x.add_argument("--spacemap-update", help="JSON object; CUSTOM only")
        x.add_argument("--json", action="store_true", help="machine-readable JSON output")
    x = sub.add_parser("route", help="show which Layout wins at a structural address")
    x.add_argument("file")
    x.add_argument("--realm", type=int, required=True)
    x.add_argument("--u", type=int, required=True)
    x.add_argument("--v", type=int)
    x.add_argument("--lane", default="NL")
    x.add_argument("--json", action="store_true", help="machine-readable JSON output")
    x = sub.add_parser("describe", help="DESCRIBE N... in address and Layout context")
    x.add_argument("file")
    x.add_argument("n")
    x.add_argument("--json", action="store_true", help="machine-readable JSON output")
    x = sub.add_parser("logical-address", help="map logical object/field through a Layout")
    x.add_argument("file")
    x.add_argument("--layout-id", type=int, required=True)
    x.add_argument("--object", type=int, required=True)
    x.add_argument("--field")
    x.add_argument("--lane", default="NL")
    x = sub.add_parser("occupied", help="list occupied cells of the logical object containing N...")
    x.add_argument("file")
    x.add_argument("n")
    x.add_argument("--as", dest="output", choices=["N", "ADDRESS", "BOTH"], default="N")
    x = sub.add_parser("stats", aliases=["info"], help="show storage and materialization statistics")
    x.add_argument("file")
    x.add_argument("--json", action="store_true", help="machine-readable JSON output")
    x = sub.add_parser("health", help="run structural and layout health checks")
    x.add_argument("file")
    x.add_argument("--deep", action="store_true")
    x.add_argument("--json", action="store_true", help="machine-readable JSON output")
    x = sub.add_parser("get", aliases=["get-n"], help="Layout-independent GET by N..., R,u,v[,lane] or u,v[,lane] with --realm")
    x.add_argument("file")
    x.add_argument("target", nargs="?", help="N123..., R1,u2,v3[,NL|NR], or u2,v3[,NL|NR] with --realm")
    x.add_argument("--realm", type=int)
    x.add_argument("--u", type=int)
    x.add_argument("--v", type=int)
    x.add_argument("--lane", default="NL", help="0/NL or 1/NR")
    x.add_argument("--mode", choices=["auto", "cell", "record"], default="auto",
                   help="cell/record are stable semantics; auto is convenience (anchor->record, member->cell)")
    x.add_argument("--json", action="store_true", help="machine-readable JSON output")
    x.add_argument("--dyn-address", choices=["off", "once", "auto"], default="off")
    x.add_argument("--dyn-v", type=int)
    x = sub.add_parser("address")
    x.add_argument("file")
    x.add_argument("--realm", type=int, required=True)
    x.add_argument("--u", type=int, required=True)
    x.add_argument("--v", type=int, required=True)
    x = sub.add_parser("cell")
    x.add_argument("file")
    x.add_argument("--realm", type=int, required=True)
    x.add_argument("--u", type=int, required=True)
    x.add_argument("--v", required=True)
    x.add_argument("--lane", default="NL", help="0/NL or 1/NR")
    x = sub.add_parser("cell-n", help="read one cell by its global N reference")
    x.add_argument("file")
    x.add_argument("n")
    x = sub.add_parser("find")
    x.add_argument("file")
    x.add_argument("--realm", type=int, required=True)
    x.add_argument("--lane", default="NL", help="0/NL oder 1/NR")
    x.add_argument("--eq", action="append")
    x.add_argument("--between", action="append")
    x.add_argument("--show", type=int, default=0)
    x.add_argument("--dyn-address", choices=["off", "once", "auto"], default="off")
    x.add_argument("--dyn-v", type=int)
    x = sub.add_parser("verify")
    x.add_argument("file")
    x.add_argument("--deep", action="store_true")
    x = sub.add_parser("rebuild-indexes")
    x.add_argument("file")
    x = sub.add_parser("repair")
    x.add_argument("file")
    x.add_argument("--apply", action="store_true")
    x.add_argument("--no-backup", action="store_true")
    x = sub.add_parser("compact")
    x.add_argument("file")
    x.add_argument("--output")
    x.add_argument("--no-backup", action="store_true")
    x.add_argument("--no-rebuild-indexes", action="store_true")
    x.add_argument("--batch-records", type=int, default=DEFAULT_BATCH_RECORDS)
    args = p.parse_args()
    if args.cmd == "to-n":
        out=cell_address(args.realm, args.u, args.v, args.lane)
        _pjson(out) if args.json else print(_human_address(out))
        return
    if args.cmd == "from-n":
        out=n_to_dynadron_address(args.n)
        _pjson(out) if args.json else print(_human_address(out, "NHyDB FROM-N"))
        return
    if args.cmd == "repair":
        _pjson(NHyDB.repair_file(args.file, apply=args.apply,
                                  make_backup=not args.no_backup))
        return
    if args.cmd == "compact":
        _pjson(NHyDB.compact_file(
            args.file, output=args.output,
            make_backup=not args.no_backup,
            rebuild_indexes=not args.no_rebuild_indexes,
            batch_records=args.batch_records,
        ))
        return
    with NHyDB(args.file) as db:
        if args.cmd in {"layout", "get-layout"}:
            out = db.get_layout(realm_id=args.realm, u=args.u, layout_id=args.layout_id)
            if args.json:
                _pjson(out)
            else:
                count=len(out) if isinstance(out,list) else (0 if out is None else 1)
                print(f"NHyDB LAYOUTS\nFile        : {args.file}\nLayouts     : {count}\n")
                print(_human_layout(out))
        elif args.cmd == "layout-check":
            out=db.layout_diagnostics(realm_id=args.realm)
            _pjson(out) if args.json else print(_human_layout_diagnostics(out))
        elif args.cmd == "layout-plan":
            out = db.plan_layout_evolution(args.layout_id, **_evolution_kwargs(args))
            _pjson(out) if args.json else print(_human_evolution_plan(out))
        elif args.cmd == "evolve-layout":
            kwargs = _evolution_kwargs(args)
            plan = db.plan_layout_evolution(args.layout_id, **kwargs)
            if not plan["safe"]:
                if args.json:
                    _pjson(plan)
                else:
                    print(_human_evolution_plan(plan))
                raise SystemExit(2)
            lay = db.evolve_layout(args.layout_id, **kwargs, commit=True)
            out = {"status": "applied", "plan": plan, "layout": db.get_layout(layout_id=lay.layout_id)}
            _pjson(out) if args.json else print(_human_evolution_plan(plan) + "\n\nApplied: YES")
        elif args.cmd == "route":
            out=db.route_address(args.realm,args.u,args.v,lane=args.lane)
            _pjson(out) if args.json else print(_human_route(out))
        elif args.cmd == "describe":
            out = db.describe_n(args.n)
            _pjson(out) if args.json else print(_human_describe(out))
        elif args.cmd == "occupied":
            _pjson(db.occupied_from_n(args.n, output=args.output))
        elif args.cmd == "logical-address":
            field = _parse_scalar(args.field) if args.field is not None else None
            _pjson(db.logical_address(args.layout_id, args.object, field, lane=args.lane))
        elif args.cmd in {"stats", "info"}:
            out = db.stats()
            _pjson(out) if args.json else print(_human_stats(out))
        elif args.cmd == "health":
            out = db.health(deep=args.deep)
            _pjson(out) if args.json else print(_human_health(out))
        elif args.cmd in {"get", "get-n"}:
            if args.target:
                if str(args.target).strip().upper().startswith("N") or str(args.target).strip().isdigit():
                    out = db.get_n(args.target, mode=args.mode)
                else:
                    rr, uu, vv, ll = _parse_structural_get_target(args.target, default_realm=args.realm)
                    out = db.get_address(rr, uu, vv, lane=ll, mode=args.mode)
                _pjson(out) if args.json else print(_human_get_result(out))
            elif args.realm is not None and args.u is not None and args.v is not None:
                out = db.get_address(args.realm, args.u, args.v, lane=args.lane, mode=args.mode)
                _pjson(out) if args.json else print(_human_get_result(out))
            elif args.realm is not None and args.u is not None:
                _pjson(db.get(args.realm, args.u, lane=args.lane,
                              dyn_address=args.dyn_address, dyn_v=args.dyn_v))
            else:
                raise SystemExit("GET braucht N..., R,u,v[,lane] oder --realm/--u/--v")
        elif args.cmd == "address":
            _pjson(db.dyn_address(args.realm, args.u, args.v))
        elif args.cmd == "cell":
            v = _parse_scalar(args.v)
            _pjson(db.get_cell(args.realm, args.u, v, lane=args.lane))
        elif args.cmd == "cell-n":
            _pjson(db.get_cell_n(args.n))
        elif args.cmd == "find":
            filters = _parse_filters(args)
            us = db.find_u(args.realm, filters, lane=args.lane)
            out = {"count": len(us), "u": us if args.show <= 0 else us[:args.show]}
            if args.show:
                out["records"] = [db.get(args.realm, u, lane=args.lane,
                                         dyn_address=args.dyn_address,
                                         dyn_v=args.dyn_v) for u in us[:args.show]]
            _pjson(out)
        elif args.cmd == "verify":
            _pjson(db.verify(deep=args.deep))
        elif args.cmd == "rebuild-indexes":
            _pjson(db.build_indexes(commit=True))
