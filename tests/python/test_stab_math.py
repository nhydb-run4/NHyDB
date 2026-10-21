"""Stabilisation: MATH parser limits (F017, T009) and constant folding (F029).

Deep inputs run in a subprocess so that a regression (stack overflow)
cannot take down the test runner.
"""
import subprocess
import sys
import textwrap

import pytest

import nhydb
import nhydb.math as rm

CHILD = textwrap.dedent(r'''
    import sys, threading
    import nhydb, nhydb.math as rm

    def outcome(src):
        try:
            e = rm.MathExpression.compile(src, ("x",))
            return ("ok", e.evaluate(x=3))
        except nhydb.MathSyntaxError as ex:
            return ("syntax", str(ex)[:40])

    CASES = {
        "par200": "(" * 200 + "x" + ")" * 200,
        "par201": "(" * 201 + "x" + ")" * 201,
        "par10000": "(" * 10000 + "x" + ")" * 10000,
        "neg200": "-" * 200 + "x",
        "neg201": "-" * 201 + "x",
        "neg100000": "-" * 100000 + "x",
        "not100000": "not " * 100000 + "x",
        "pow100000": "**".join(["x"] * 100000),
        "abs5000": "abs(" * 5000 + "x" + ")" * 5000,
        "sum100000": "x" + "+x" * 99999,
    }
    results = {}
    def run():
        for k, src in CASES.items():
            results[k] = outcome(src)
    run()
    main = dict(results)
    t = threading.Thread(target=run)
    t.start(); t.join()
    assert results == main, (results, main)
    for k, v in sorted(main.items()):
        print(k, v[0], v[1] if v[0] == "ok" else "")
''')


def test_parser_depth_limits_subprocess():
    r = subprocess.run([sys.executable, "-c", CHILD], capture_output=True, text=True, timeout=300)
    assert r.returncode == 0, r.stderr[-2000:]
    got = dict(line.split(" ", 1) for line in r.stdout.strip().splitlines())
    assert got["par200"].startswith("ok 3")
    assert got["par201"].startswith("syntax")
    assert got["par10000"].startswith("syntax")
    assert got["neg200"].startswith("ok 3")
    for k in ("neg201", "neg100000", "not100000", "pow100000", "abs5000"):
        assert got[k].startswith("syntax"), (k, got[k])
    assert got["sum100000"].startswith("ok 300000")


def test_paren_message_matches_python():
    with pytest.raises(nhydb.MathSyntaxError, match="too many nested parentheses"):
        rm.MathExpression.compile("(" * 201 + "x" + ")" * 201, ("x",))


def test_math_map_layout_rejects_deep_formula(tmp_path):
    db = nhydb.NHyDB.create(str(tmp_path / "m.nhydb"))
    with pytest.raises(nhydb.MathSyntaxError):
        db.create_math_map_layout("m", ("x", "y"), u="-" * 5000 + "x", V="y")
    db.close()


def test_constant_folding_respects_limit():
    m = rm.MathMap.compile("t", ("x",), u="2**200//2**190 + x", V="1", side="NL")
    old = rm.get_math_max_intermediate_bits()
    try:
        rm.set_math_max_intermediate_bits(128)
        for x in (5, 2**70):
            with pytest.raises(nhydb.MathDomainError):
                m.target(x)
        with pytest.raises(nhydb.MathDomainError):
            rm.MathExpression.compile("2**200//2**190 + x", ("x",)).evaluate(x=5)
    finally:
        rm.set_math_max_intermediate_bits(old)
    assert m.target(5).u == 1029


def test_math_bit_limit_frozen_per_layout(tmp_path):
    # F030: written with a large limit, reopened with a small one → still readable
    p = tmp_path / "f.nhydb"
    big = tmp_path / "big.toml"
    small = tmp_path / "small.toml"
    big.write_text("[math]\nmath_max_intermediate_bits = 1048576\n")
    small.write_text("[math]\nmath_max_intermediate_bits = 200\n")
    db = nhydb.NHyDB.create(str(p), config=str(big))
    st = db.create_math_map_layout("m", ("x", "y"), u="x*x*x*x", V="y", payload_type="utf8")
    assert st.max_intermediate_bits == 1048576
    x = 2**100
    db.put_math(st.layout_id, (x, 1), "v", commit=True)
    db.close()
    db = nhydb.NHyDB(str(p), config=str(small))
    assert db.get_math(st.layout_id, (x, 1)) == "v"
    assert db.math_layouts[st.layout_id].max_intermediate_bits == 1048576
    # a layout created in this session freezes the small limit
    st2 = db.create_math_map_layout("m2", ("x", "y"), u="x*x*x*x", V="y", payload_type="utf8")
    assert st2.max_intermediate_bits == 200
    with pytest.raises(nhydb.MathDomainError):
        db.put_math(st2.layout_id, (x, 1), "w")
    db.close()


@pytest.mark.filterwarnings("ignore::SyntaxWarning")
@pytest.mark.parametrize("src", ["0x_ff + x", "0b_1 + x", "0o_7_7 + x", "x\n# c", "x\n\n  # c\n", "x # c\n# d",
                                 "1and x", "1or x", "(x\n)", "x\\\n+1", "1_000 + x", "0_0 + x",
                                 "0xfor x"])
def test_lexer_accepts_python_valid_f042(src):
    import ast
    ast.parse(src.replace("^", "**"), mode="eval")  # valid Python
    e = rm.MathExpression.compile(src, ("x",))
    assert e.evaluate(x=3) is not None


@pytest.mark.filterwarnings("ignore::SyntaxWarning")
@pytest.mark.parametrize("src", ["0x__ff", "0x_", "x\n y", "1andy", "1abc", "0b12", "1__0"])
def test_lexer_rejects_python_invalid_f042(src):
    import ast
    with pytest.raises(SyntaxError):
        ast.parse(src, mode="eval")
    with pytest.raises(nhydb.MathSyntaxError):
        rm.MathExpression.compile(src, ("x",))


def test_nfc_matches_cpython_f043():
    import random
    import unicodedata

    from nhydb import _native as n

    if unicodedata.unidata_version != "16.0.0":
        pytest.skip("engine tables are Unicode 16.0.0")
    for cp in range(0x110000):
        if 0xD800 <= cp <= 0xDFFF:
            continue
        c = chr(cp)
        assert n._unicode_map("nfc", c) == unicodedata.normalize("NFC", c), hex(cp)
    marks = [chr(cp) for cp in range(0x300, 0x370)] + ["̣", "்", "़", "ְ", "゙"]
    bases = ["a", "e", "o", "u", "A", "é", "ᄀ", "ᅡ", "ᆨ", "가", "क", "か", "א"]
    rnd = random.Random(7)
    for _ in range(20000):
        s = "".join(rnd.choice(bases + marks) for _ in range(rnd.randint(1, 6)))
        assert n._unicode_map("nfc", s) == unicodedata.normalize("NFC", s), ascii(s)


def test_identifier_not_nfc_rejected_f043():
    decomposed = "é"  # é as e + COMBINING ACUTE
    with pytest.raises(nhydb.MathSyntaxError, match="NFKC"):
        rm.MathExpression.compile(decomposed + " + 1", (decomposed,))
    with pytest.raises(nhydb.MathSyntaxError):
        rm.MathExpression.compile("é + 1", (decomposed,))
    assert rm.MathExpression.compile("é + 1", ("é",)).evaluate(**{"é": 2}) == 3
