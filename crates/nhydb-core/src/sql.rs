//! NHySQL-0.24: lesende SELECT-Sprache (Python `nhydb_query_v027`, Teil SQL).
//!
//! Lexer, rekursiver Abstiegsparser, Übersetzung in den Abfragebaum
//! (`query::Expr`) und Ausführung über die native QueryEngine. Positionen,
//! Zeilen/Spalten und `near`-Ausschnitte der Fehlermeldungen zählen wie
//! Python in Code Points; Zeichenklassen (`isspace`, `isdigit`, `isalpha`,
//! `isalnum`) und Zahlumwandlung (`int()`/`float()` mit Unicode-Dezimalziffern)
//! folgen CPython.

use std::collections::HashMap;

use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive};

use crate::address::Lane;
use crate::db::layout::LayoutRef;
use crate::db::Db;
use crate::error::{Error, Result};
use crate::nat::Nat;
use crate::query::{self, Expr, ExecOptions, QueryEngine, QUERY_VERSION};
use crate::schema::{LayoutDef, LayoutKind};
use crate::unicode;
use crate::val::Val;
use crate::vmap;

pub const SQL_LANGUAGE_VERSION: &str = "NHySQL-0.24";

fn syn(m: impl Into<String>) -> Error {
    Error::SqlSyntax(m.into())
}

fn repr(s: &str) -> String {
    Val::str(s).py_repr()
}

const CLAUSE_WORDS: &[&str] = &[
    "FROM", "WHERE", "GROUP", "ORDER", "LIMIT", "OFFSET", "HAVING", "UNION", "INTERSECT", "EXCEPT", "WINDOW", "FETCH", "FOR", "AS",
    "ASC", "DESC", "AND", "OR", "NOT", "BETWEEN", "IS", "NULL",
];
const AGGREGATES: &[&str] = &["COUNT", "SUM", "MIN", "MAX", "AVG"];
const TEXT_FUNCS: &[&str] = &["TOKEN", "TOKENS_ALL", "TOKENS_ANY", "CONTAINS", "ICONTAINS", "FULLTEXT", "FULLTEXT_ANY", "PHRASE", "EXISTS"];
const STRUCT_NAMES: &[&str] = &["realm", "u", "v", "lane", "n", "record_n", "layout_id", "layout_kind", "layout_name", "object"];

// ----------------------------------------------------------------------
// Lexer
// ----------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokKind {
    Str,
    Ident,
    Symbol,
    Param,
    NamedParam,
    Number,
    Eof,
}

impl TokKind {
    pub fn name(self) -> &'static str {
        match self {
            TokKind::Str => "STRING",
            TokKind::Ident => "IDENT",
            TokKind::Symbol => "SYMBOL",
            TokKind::Param => "PARAM",
            TokKind::NamedParam => "NAMED_PARAM",
            TokKind::Number => "NUMBER",
            TokKind::Eof => "EOF",
        }
    }
}

/// `SQLToken(kind, value, pos)`; `pos` in Code Points.
#[derive(Clone, Debug, PartialEq)]
pub struct SqlToken {
    pub kind: TokKind,
    pub value: Val,
    pub pos: usize,
}

fn tok(kind: TokKind, value: Val, pos: usize) -> SqlToken {
    SqlToken { kind, value, pos }
}

fn ident_char(c: char) -> bool {
    unicode::is_alnum(c) || c == '_' || c == '$'
}

/// Ziffernfolge (`str.isdigit`-Zeichen, Vorzeichen, Punkt, Exponent) → ASCII,
/// wie `int()`/`float()` sie lesen; nicht-dezimale Ziffern (z. B. `²`) ⇒ None.
fn ascii_number(raw: &[char]) -> Option<String> {
    let mut s = String::with_capacity(raw.len());
    for &c in raw {
        if c.is_ascii() {
            s.push(c);
        } else {
            s.push((b'0' + unicode::decimal_value(c)?) as char);
        }
    }
    Some(s)
}

/// `_sql_scan`
pub fn scan(text: &str) -> Result<Vec<SqlToken>> {
    let t: Vec<char> = text.chars().collect();
    let n = t.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut param_no = 0i64;
    let starts = |i: usize, p: &str| -> bool {
        let pc: Vec<char> = p.chars().collect();
        i + pc.len() <= n && t[i..i + pc.len()] == pc[..]
    };
    let find = |from: usize, p: &str| -> Option<usize> { (from..n).find(|&j| starts(j, p)) };
    while i < n {
        let ch = t[i];
        if unicode::is_space(ch) {
            i += 1;
            continue;
        }
        if starts(i, "--") {
            i = match find(i + 2, "\n") {
                None => n,
                Some(j) => j + 1,
            };
            continue;
        }
        if starts(i, "/*") {
            match find(i + 2, "*/") {
                None => return Err(syn("nicht abgeschlossener /* ... */ Kommentar")),
                Some(j) => i = j + 2,
            }
            continue;
        }
        if ch == '\'' || ch == '"' || ch == '`' {
            let quote = ch;
            let start = i;
            i += 1;
            let mut parts = String::new();
            let mut closed = false;
            while i < n {
                if t[i] == quote {
                    if i + 1 < n && t[i + 1] == quote {
                        parts.push(quote);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    closed = true;
                    break;
                }
                parts.push(t[i]);
                i += 1;
            }
            if !closed {
                return Err(syn(if quote == '\'' { "nicht abgeschlossener Stringliteral" } else { "nicht abgeschlossener Identifier" }));
            }
            out.push(tok(if quote == '\'' { TokKind::Str } else { TokKind::Ident }, Val::Str(parts), start));
            continue;
        }
        if i + 1 < n {
            let two: String = t[i..i + 2].iter().collect();
            if matches!(two.as_str(), "<=" | ">=" | "!=" | "<>") {
                out.push(tok(TokKind::Symbol, Val::Str(two), i));
                i += 2;
                continue;
            }
        }
        if "(),.*;=<>".contains(ch) {
            out.push(tok(TokKind::Symbol, Val::Str(ch.to_string()), i));
            i += 1;
            continue;
        }
        if ch == '?' {
            out.push(tok(TokKind::Param, Val::Int(param_no), i));
            param_no += 1;
            i += 1;
            continue;
        }
        if ch == ':' || ch == '@' {
            let start = i;
            i += 1;
            let mut j = i;
            while j < n && ident_char(t[j]) {
                j += 1;
            }
            if j == i {
                return Err(syn(if ch == ':' {
                    format!("ungueltiger benannter Parameter an Position {start}")
                } else {
                    format!("ungueltiger Strukturbegriff an Position {start}")
                }));
            }
            let name: String = t[i..j].iter().collect();
            out.push(if ch == ':' { tok(TokKind::NamedParam, Val::Str(name), start) } else { tok(TokKind::Ident, Val::Str(format!("@{name}")), start) });
            i = j;
            continue;
        }
        if unicode::is_digit(ch) || ((ch == '+' || ch == '-') && i + 1 < n && unicode::is_digit(t[i + 1])) {
            let start = i;
            i += 1;
            let (mut has_dot, mut has_exp) = (false, false);
            while i < n {
                let c = t[i];
                if unicode::is_digit(c) {
                    i += 1;
                    continue;
                }
                if c == '.' && !has_dot && !has_exp {
                    has_dot = true;
                    i += 1;
                    continue;
                }
                if (c == 'e' || c == 'E') && !has_exp {
                    has_exp = true;
                    i += 1;
                    if i < n && (t[i] == '+' || t[i] == '-') {
                        i += 1;
                    }
                    continue;
                }
                break;
            }
            let raw: String = t[start..i].iter().collect();
            let bad = || syn(format!("ungueltige Zahl {}", repr(&raw)));
            let ascii = ascii_number(&t[start..i]).ok_or_else(bad)?;
            let value = if has_dot || has_exp {
                Val::Float(py_float_literal(&ascii).ok_or_else(bad)?)
            } else {
                Val::from_bigint(ascii.parse::<BigInt>().map_err(|_| bad())?)
            };
            out.push(tok(TokKind::Number, value, start));
            continue;
        }
        if unicode::is_alpha(ch) || ch == '_' {
            let start = i;
            i += 1;
            while i < n && ident_char(t[i]) {
                i += 1;
            }
            out.push(tok(TokKind::Ident, Val::Str(t[start..i].iter().collect()), start));
            continue;
        }
        return Err(syn(format!("unbekanntes Zeichen {} an Position {i}", repr(&ch.to_string()))));
    }
    out.push(tok(TokKind::Eof, Val::str(""), n));
    Ok(out)
}

/// Python `float()` für die vom Lexer erzeugten Formen (`1.`, `1e5`, `1.e-3`).
fn py_float_literal(s: &str) -> Option<f64> {
    let body = s.trim_start_matches(['+', '-']);
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(p) => (&body[..p], Some(&body[p + 1..])),
        None => (body, None),
    };
    if mant.is_empty() || mant == "." || !mant.chars().next().unwrap().is_ascii_digit() {
        return None;
    }
    if let Some(e) = exp {
        let e = e.trim_start_matches(['+', '-']);
        if e.is_empty() || !e.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
    }
    s.parse::<f64>().ok().or_else(|| {
        // Rust akzeptiert "1." und "1.e5" nicht in jeder Form: Punkt ergänzen
        let fixed = s.replacen(".e", ".0e", 1).replacen(".E", ".0E", 1);
        let fixed = if fixed.ends_with('.') { format!("{fixed}0") } else { fixed };
        fixed.parse::<f64>().ok()
    })
}

// ----------------------------------------------------------------------
// AST
// ----------------------------------------------------------------------

/// `SQLSelectItem(kind, name, op, alias)`
#[derive(Clone, Debug, PartialEq)]
pub struct SelectItem {
    pub kind: String,
    pub name: String,
    pub op: String,
    pub alias: Option<String>,
}

impl SelectItem {
    fn new(kind: &str, name: &str, op: &str, alias: Option<String>) -> Self {
        SelectItem { kind: kind.into(), name: name.into(), op: op.into(), alias }
    }
}

/// `SQLOrderItem(name, direction)`
#[derive(Clone, Debug, PartialEq)]
pub struct OrderItem {
    pub name: String,
    pub direction: String,
}

/// `SQLSelectStatement`
#[derive(Clone, Debug, PartialEq)]
pub struct SelectStatement {
    pub source: Vec<String>,
    pub source_alias: Option<String>,
    pub select_items: Vec<SelectItem>,
    pub where_: Expr,
    pub group_by: Vec<String>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<BigInt>,
    pub offset: BigInt,
    pub explain: bool,
    pub text: String,
}

/// `SQLCompiledQuery`
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledQuery {
    pub statement: SelectStatement,
    pub layout_id: u64,
    pub layout_name: String,
    pub realm: Nat,
    pub lane: Lane,
    pub expr: Expr,
    pub return_mode: String,
    pub select: Vec<String>,
    /// `{alias: {"op", "field"}}`
    pub aggregate: Val,
    pub group_by: Vec<String>,
    pub order_by: Vec<(String, String)>,
    pub limit: Option<BigInt>,
    pub offset: BigInt,
    pub ranked: bool,
    pub output: Vec<SelectItem>,
    pub all_fields: Vec<String>,
}

// ----------------------------------------------------------------------
// Parser
// ----------------------------------------------------------------------

/// SQL-Parameter: Sequenz (`?`) oder Mapping (`:name`).
#[derive(Clone, Debug)]
pub enum Params {
    None,
    Seq(Vec<Val>),
    Map(Vec<(String, Val)>),
}

enum Ref {
    Field(String),
    Struct(String),
}

impl Ref {
    fn pred(&self, op: &str, value: Val, value2: Val, options: Val) -> Expr {
        match self {
            Ref::Field(n) => Expr::Field { name: n.clone(), op: op.into(), value, value2, options },
            Ref::Struct(n) => Expr::Struct { name: n.clone(), op: op.into(), value, value2 },
        }
    }
    fn simple(&self, op: &str, value: Val) -> Expr {
        self.pred(op, value, Val::None, Val::map())
    }
    fn contains(&self, v: Val, casefold: bool) -> Expr {
        self.pred(if casefold { "icontains" } else { "contains" }, v, Val::None, vmap! {"ngram" => 3i64, "casefold" => casefold})
    }
    fn tokens(&self, op: &str, v: Val) -> Expr {
        self.pred(op, v, Val::None, vmap! {"casefold" => true, "tokenizer" => "simple"})
    }
    fn fulltext(&self, v: Val, mode: &str) -> Expr {
        self.pred("fulltext", v, Val::None, vmap! {"mode" => mode, "casefold" => true, "tokenizer" => "simple", "positions" => true})
    }
    fn phrase(&self, v: Val) -> Expr {
        self.pred("phrase", v, Val::None, vmap! {"casefold" => true, "tokenizer" => "simple", "positions" => true})
    }
}

struct Parser<'p> {
    chars: Vec<char>,
    text: String,
    toks: Vec<SqlToken>,
    i: usize,
    params: &'p Params,
    source: Vec<String>,
    source_alias: Option<String>,
}

fn tval(t: &SqlToken) -> String {
    t.value.py_str()
}

impl Parser<'_> {
    fn tok(&self, off: usize) -> &SqlToken {
        let j = (self.i + off).min(self.toks.len() - 1);
        &self.toks[j]
    }
    fn at_kw(&self, w: &str) -> bool {
        let t = self.tok(0);
        t.kind == TokKind::Ident && unicode::upper(&tval(t)) == w
    }
    fn match_kw(&mut self, w: &str) -> bool {
        if self.at_kw(w) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect_kw(&mut self, w: &str) -> Result<()> {
        if !self.match_kw(w) {
            return Err(self.error(&format!("{w} erwartet"), None));
        }
        Ok(())
    }
    fn at_sym(&self, s: &str) -> bool {
        let t = self.tok(0);
        t.kind == TokKind::Symbol && tval(t) == s
    }
    fn match_sym(&mut self, s: &str) -> bool {
        if self.at_sym(s) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect_sym(&mut self, s: &str) -> Result<()> {
        if !self.match_sym(s) {
            return Err(self.error(&format!("{} erwartet", repr(s)), None));
        }
        Ok(())
    }

    /// `_error`: Meldung mit Zeile, Spalte und `near`-Ausschnitt.
    fn error(&self, msg: &str, t: Option<&SqlToken>) -> Error {
        let pos = t.unwrap_or(self.tok(0)).pos.min(self.chars.len());
        let before = &self.chars[..pos];
        let line = before.iter().filter(|&&c| c == '\n').count() + 1;
        let col = match before.iter().rposition(|&c| c == '\n') {
            None => pos + 1,
            Some(last) => pos - last,
        };
        let end = (pos + 32).min(self.chars.len());
        let near: String = self.chars[pos..end].iter().take_while(|&&c| c != '\n').collect();
        syn(format!("{msg} (Zeile {line}, Spalte {col}, bei {})", repr(&near)))
    }

    fn identifier(&mut self) -> Result<String> {
        let t = self.tok(0).clone();
        if t.kind != TokKind::Ident && t.kind != TokKind::Number {
            return Err(self.error("Identifier erwartet", None));
        }
        self.i += 1;
        Ok(tval(&t))
    }

    fn identifier_path(&mut self) -> Result<String> {
        let mut parts = vec![self.identifier()?];
        while self.match_sym(".") {
            if self.at_sym("*") {
                self.i += 1;
                parts.push("*".into());
                break;
            }
            parts.push(self.identifier()?);
        }
        Ok(parts.join("."))
    }

    fn literal(&mut self) -> Result<Val> {
        let t = self.tok(0).clone();
        match t.kind {
            TokKind::Str | TokKind::Number => {
                self.i += 1;
                Ok(t.value)
            }
            TokKind::Param => {
                self.i += 1;
                let idx = t.value.as_i64().unwrap_or(0) as usize;
                let missing = || syn(format!("Wert fuer Parameter ?{} fehlt", idx + 1));
                match self.params {
                    Params::None | Params::Map(_) => Err(self.error("? Parameter braucht eine Sequenz von Werten", Some(&t))),
                    Params::Seq(v) => v.get(idx).cloned().ok_or_else(missing),
                }
            }
            TokKind::NamedParam => {
                self.i += 1;
                let name = tval(&t);
                match self.params {
                    Params::Map(m) => m
                        .iter()
                        .find(|(k, _)| *k == name)
                        .map(|(_, v)| v.clone())
                        .ok_or_else(|| syn(format!("Wert fuer Parameter :{name} fehlt"))),
                    _ => Err(self.error(&format!(":{name} braucht ein Mapping von Parametern"), Some(&t))),
                }
            }
            TokKind::Ident => {
                let up = unicode::upper(&tval(&t));
                let v = match up.as_str() {
                    "TRUE" => Val::Bool(true),
                    "FALSE" => Val::Bool(false),
                    "NULL" => Val::None,
                    _ => return Err(self.error("Literal erwartet", None)),
                };
                self.i += 1;
                Ok(v)
            }
            _ => Err(self.error("Literal erwartet", None)),
        }
    }

    fn implicit_alias_allowed(t: &SqlToken) -> bool {
        t.kind == TokKind::Ident && !CLAUSE_WORDS.contains(&unicode::upper(&tval(t)).as_str())
    }

    fn optional_alias(&mut self) -> Result<Option<String>> {
        if self.match_kw("AS") {
            return Ok(Some(self.identifier()?));
        }
        if Self::implicit_alias_allowed(self.tok(0)) {
            return Ok(Some(self.identifier()?));
        }
        Ok(None)
    }

    fn parse(&mut self) -> Result<SelectStatement> {
        let explain = self.match_kw("EXPLAIN");
        if explain && self.match_kw("QUERY") {
            self.expect_kw("PLAN")?;
        }
        self.expect_kw("SELECT")?;
        if self.match_kw("DISTINCT") {
            return Err(self.error("DISTINCT ist in NHySQL V0.27 noch nicht implementiert", None));
        }
        let items = self.select_list()?;
        self.expect_kw("FROM")?;
        self.source = self.source_ref()?;
        if self.match_kw("AS")
            || (Self::implicit_alias_allowed(self.tok(0))
                && !["WHERE", "GROUP", "ORDER", "LIMIT", "OFFSET"].contains(&unicode::upper(&tval(self.tok(0))).as_str()))
        {
            self.source_alias = Some(self.identifier()?);
        }
        let mut where_ = Expr::All;
        if self.match_kw("WHERE") {
            where_ = self.or_expr()?;
        }
        let mut group_by = Vec::new();
        if self.match_kw("GROUP") {
            self.expect_kw("BY")?;
            group_by.push(self.identifier_path()?);
            while self.match_sym(",") {
                group_by.push(self.identifier_path()?);
            }
        }
        if self.match_kw("HAVING") {
            return Err(self.error("HAVING ist in NHySQL V0.27 noch nicht implementiert", None));
        }
        let mut order_by = Vec::new();
        if self.match_kw("ORDER") {
            self.expect_kw("BY")?;
            loop {
                let name = self.identifier_path()?;
                let mut direction = "asc";
                if self.match_kw("ASC") {
                    direction = "asc";
                } else if self.match_kw("DESC") {
                    direction = "desc";
                }
                order_by.push(OrderItem { name, direction: direction.into() });
                if !self.match_sym(",") {
                    break;
                }
            }
        }
        let mut limit = None;
        let mut offset = BigInt::from(0);
        let nonneg = |v: &Val| -> Option<BigInt> {
            match v {
                Val::Int(_) | Val::Big(_) => v.as_bigint().filter(|b| !b.is_negative()),
                _ => None,
            }
        };
        if self.match_kw("LIMIT") {
            let v = self.literal()?;
            limit = Some(nonneg(&v).ok_or_else(|| self.error("LIMIT braucht einen nichtnegativen Integer", None))?);
        }
        if self.match_kw("OFFSET") {
            let v = self.literal()?;
            offset = nonneg(&v).ok_or_else(|| self.error("OFFSET braucht einen nichtnegativen Integer", None))?;
        }
        self.match_sym(";");
        if self.tok(0).kind != TokKind::Eof {
            return Err(self.error("nur ein SELECT-Statement pro Aufruf ist erlaubt", None));
        }
        Ok(SelectStatement {
            source: self.source.clone(),
            source_alias: self.source_alias.clone(),
            select_items: items,
            where_,
            group_by,
            order_by,
            limit,
            offset,
            explain,
            text: self.text.clone(),
        })
    }

    fn source_ref(&mut self) -> Result<Vec<String>> {
        let mut parts = vec![self.identifier()?];
        if self.match_sym(".") {
            parts.push(self.identifier()?);
            if self.match_sym(".") {
                return Err(self.error("FROM unterstuetzt hoechstens realm.layout", None));
            }
        }
        Ok(parts)
    }

    fn select_list(&mut self) -> Result<Vec<SelectItem>> {
        let mut out = Vec::new();
        loop {
            if self.match_sym("*") {
                out.push(SelectItem::new("star", "", "", None));
            } else {
                let t = self.tok(0).clone();
                if t.kind != TokKind::Ident {
                    return Err(self.error("Feld, * oder Aggregat im SELECT erwartet", None));
                }
                let upper = unicode::upper(&tval(&t));
                let next = self.tok(1);
                if AGGREGATES.contains(&upper.as_str()) && next.kind == TokKind::Symbol && tval(next) == "(" {
                    self.i += 1;
                    self.expect_sym("(")?;
                    let field = if self.match_sym("*") { "*".to_string() } else { self.identifier_path()? };
                    self.expect_sym(")")?;
                    let alias = match self.optional_alias()? {
                        Some(a) => a,
                        None => {
                            let arg = if field == "*" { "all".to_string() } else { field.rsplit('.').next().unwrap().to_string() };
                            format!("{}_{arg}", unicode::lower(&upper))
                        }
                    };
                    out.push(SelectItem::new("aggregate", &field, &unicode::lower(&upper), Some(alias)));
                } else {
                    let name = self.identifier_path()?;
                    if name.ends_with(".*") {
                        out.push(SelectItem::new("star", &name, "", None));
                    } else {
                        let alias = self.optional_alias()?;
                        let kind = if unicode::casefold(name.rsplit('.').next().unwrap()) == "relevance" { "relevance" } else { "field" };
                        out.push(SelectItem::new(kind, &name, "", alias));
                    }
                }
            }
            if !self.match_sym(",") {
                break;
            }
        }
        Ok(out)
    }

    fn or_expr(&mut self) -> Result<Expr> {
        let mut out = self.and_expr()?;
        while self.match_kw("OR") {
            out = Expr::or(out, self.and_expr()?);
        }
        Ok(out)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut out = self.not_expr()?;
        while self.match_kw("AND") {
            out = Expr::and(out, self.not_expr()?);
        }
        Ok(out)
    }

    fn not_expr(&mut self) -> Result<Expr> {
        if self.match_kw("NOT") {
            return Ok(Expr::negate(self.not_expr()?));
        }
        self.predicate_primary()
    }

    fn predicate_primary(&mut self) -> Result<Expr> {
        if self.match_sym("(") {
            let out = self.or_expr()?;
            self.expect_sym(")")?;
            return Ok(out);
        }
        let t = self.tok(0).clone();
        let next = self.tok(1).clone();
        if t.kind == TokKind::Ident && TEXT_FUNCS.contains(&unicode::upper(&tval(&t)).as_str()) && next.kind == TokKind::Symbol && tval(&next) == "(" {
            return self.function_predicate();
        }
        let name = self.identifier_path()?;
        let r = self.reference(&name)?;
        if self.match_kw("IS") {
            let neg = self.match_kw("NOT");
            self.expect_kw("NULL")?;
            let e = r.simple("exists", Val::None);
            return Ok(if neg { e } else { Expr::negate(e) });
        }
        if self.match_kw("BETWEEN") {
            let lo = self.literal()?;
            self.expect_kw("AND")?;
            let hi = self.literal()?;
            return Ok(r.pred("between", lo, hi, Val::map()));
        }
        for w in ["CONTAINS", "ICONTAINS", "TOKEN", "FULLTEXT", "PHRASE"] {
            if self.match_kw(w) {
                let v = self.literal()?;
                return Ok(match w {
                    "CONTAINS" => r.contains(v, false),
                    "ICONTAINS" => r.contains(v, true),
                    "TOKEN" => r.tokens("token", v),
                    "FULLTEXT" => r.fulltext(v, "all"),
                    _ => r.phrase(v),
                });
            }
        }
        let op = self.tok(0).clone();
        let sym = tval(&op);
        if op.kind != TokKind::Symbol || !["=", "!=", "<>", "<", "<=", ">", ">="].contains(&sym.as_str()) {
            return Err(self.error("Vergleichsoperator, BETWEEN, IS NULL oder Textoperator erwartet", None));
        }
        self.i += 1;
        let value = self.literal()?;
        if value.is_none() {
            return Err(self.error("NULL mit IS NULL / IS NOT NULL vergleichen", Some(&op)));
        }
        let o = match sym.as_str() {
            "=" => "eq",
            "!=" | "<>" => "ne",
            "<" => "lt",
            "<=" => "le",
            ">" => "gt",
            _ => "ge",
        };
        Ok(r.simple(o, value))
    }

    fn function_predicate(&mut self) -> Result<Expr> {
        let func = unicode::upper(&self.identifier()?);
        self.expect_sym("(")?;
        let field = self.identifier_path()?;
        let r = self.reference(&field)?;
        if func == "EXISTS" {
            self.expect_sym(")")?;
            return Ok(r.simple("exists", Val::None));
        }
        self.expect_sym(",")?;
        let mut values = vec![self.literal()?];
        while self.match_sym(",") {
            values.push(self.literal()?);
        }
        self.expect_sym(")")?;
        let one = |p: &Parser<'_>, msg: &str| -> Result<Val> {
            if values.len() != 1 {
                return Err(p.error(msg, None));
            }
            Ok(values[0].clone())
        };
        Ok(match func.as_str() {
            "TOKEN" => r.tokens("token", one(self, "TOKEN(field, value) erwartet genau einen Wert")?),
            "TOKENS_ALL" => r.tokens("tokens_all", Val::List(values.clone())),
            "TOKENS_ANY" => r.tokens("tokens_any", Val::List(values.clone())),
            "CONTAINS" => r.contains(one(self, "CONTAINS(field, value) erwartet genau einen Wert")?, false),
            "ICONTAINS" => r.contains(one(self, "ICONTAINS(field, value) erwartet genau einen Wert")?, true),
            "FULLTEXT" => r.fulltext(one(self, "FULLTEXT(field, query) erwartet genau einen Suchtext")?, "all"),
            "FULLTEXT_ANY" => r.fulltext(one(self, "FULLTEXT_ANY(field, query) erwartet genau einen Suchtext")?, "any"),
            "PHRASE" => r.phrase(one(self, "PHRASE(field, phrase) erwartet genau eine Phrase")?),
            other => return Err(self.error(&format!("unbekannte Praedikatfunktion {other}"), None)),
        })
    }

    /// `_ref`: `@name`, `STRUCT.name`, `qualifier.feld` oder `feld`.
    fn reference(&self, raw: &str) -> Result<Ref> {
        let parts: Vec<&str> = raw.split('.').collect();
        if parts.len() == 1 && parts[0].starts_with('@') {
            let name = &parts[0][1..];
            if !STRUCT_NAMES.contains(&unicode::casefold(name).as_str()) {
                return Err(self.error(&format!("unbekannter Strukturbegriff @{name}"), None));
            }
            return Ok(Ref::Struct(canonical_struct_name(name)));
        }
        if parts.len() == 2 && unicode::casefold(parts[0]) == "struct" {
            if !STRUCT_NAMES.contains(&unicode::casefold(parts[1]).as_str()) {
                return Err(self.error(&format!("unbekannter Strukturbegriff STRUCT.{}", parts[1]), None));
            }
            return Ok(Ref::Struct(canonical_struct_name(parts[1])));
        }
        if parts.len() == 2 {
            let q = unicode::casefold(parts[0]);
            let ok = self.source.iter().any(|s| unicode::casefold(s) == q) || self.source_alias.as_ref().is_some_and(|a| unicode::casefold(a) == q);
            if !ok {
                return Err(self.error(&format!("unbekannter Tabellenalias/Qualifier {}", repr(parts[0])), None));
            }
            return Ok(Ref::Field(parts[1].to_string()));
        }
        if parts.len() > 2 {
            return Err(self.error("Feldreferenz darf hoechstens qualifier.feld enthalten", None));
        }
        Ok(Ref::Field(raw.to_string()))
    }
}

fn canonical_struct_name(name: &str) -> String {
    let low = unicode::casefold(name);
    match low.as_str() {
        "n" => "N".into(),
        "record_n" => "record_N".into(),
        _ => low,
    }
}

/// `parse_sql(text, parameters=None)`
pub fn parse_sql(text: &str, params: &Params) -> Result<SelectStatement> {
    let toks = scan(text)?;
    let mut p = Parser { chars: text.chars().collect(), text: text.to_string(), toks, i: 0, params, source: Vec::new(), source_alias: None };
    p.parse()
}

// ----------------------------------------------------------------------
// Übersetzung (compile_sql)
// ----------------------------------------------------------------------

fn accepted_qualifier(stmt: &SelectStatement, q: &str) -> bool {
    let q = unicode::casefold(q);
    stmt.source.iter().any(|s| unicode::casefold(s) == q) || stmt.source_alias.as_ref().is_some_and(|a| unicode::casefold(a) == q)
}

/// `_sql_unqualify`
fn unqualify(name: &str, stmt: &SelectStatement) -> Result<String> {
    let parts: Vec<&str> = name.split('.').collect();
    match parts.len() {
        1 => Ok(parts[0].to_string()),
        2 => {
            if !accepted_qualifier(stmt, parts[0]) {
                return Err(syn(format!("unbekannter Tabellenalias/Qualifier {}", repr(parts[0]))));
            }
            Ok(parts[1].to_string())
        }
        _ => Err(syn(format!("ungueltige Feldreferenz {}", repr(name)))),
    }
}

fn is_ascii_digits(s: &str) -> bool {
    // Python `str.isdigit()` (Unicode-Ziffern eingeschlossen)
    !s.is_empty() && s.chars().all(unicode::is_digit)
}

fn py_int_digits(s: &str) -> Result<BigInt> {
    let chars: Vec<char> = s.chars().collect();
    let ascii = ascii_number(&chars).ok_or_else(|| Error::Value(format!("invalid literal for int() with base 10: {}", repr(s))))?;
    ascii.parse::<BigInt>().map_err(|_| Error::Value(format!("invalid literal for int() with base 10: {}", repr(s))))
}

/// `_sql_resolve_source`: `layout` oder `realm.layout`; nur CLASSIC.
pub fn resolve_source<'d>(db: &'d Db, source: &[String]) -> Result<&'d LayoutDef> {
    if source.is_empty() || source.len() > 2 {
        return Err(syn("FROM erwartet layout oder realm.layout"));
    }
    let realm_id_for = |raw: &str| -> Result<Option<BigInt>> {
        let low = unicode::casefold(raw);
        for prefix in ["realm_", "realm", "r"] {
            if let Some(rest) = low.strip_prefix(prefix) {
                if is_ascii_digits(rest) {
                    return Ok(Some(py_int_digits(rest)?));
                }
            }
        }
        if is_ascii_digits(raw) {
            return Ok(Some(py_int_digits(raw)?));
        }
        let matches: Vec<&Nat> = db.realms.iter().filter(|(_, r)| unicode::casefold(&r.name) == low).map(|(k, _)| k).collect();
        match matches.len() {
            1 => Ok(Some(matches[0].to_bigint())),
            0 => Ok(None),
            _ => Err(syn(format!("Realmname {} ist nicht eindeutig", repr(raw)))),
        }
    };
    let mut realm_filter: Option<Nat> = None;
    let layout_name = source.last().unwrap();
    if source.len() == 2 {
        let rf = realm_id_for(&source[0])?;
        let rn = rf.as_ref().and_then(Nat::from_bigint).filter(|n| db.realms.contains_key(n));
        match rn {
            Some(n) => realm_filter = Some(n),
            None => return Err(syn(format!("Realm {} nicht gefunden", repr(&source[0])))),
        }
    }
    let low = unicode::casefold(layout_name);
    let lid: Option<BigInt> = if is_ascii_digits(layout_name) {
        Some(py_int_digits(layout_name)?)
    } else if let Some(rest) = low.strip_prefix("layout_").filter(|r| is_ascii_digits(r)) {
        Some(py_int_digits(rest)?)
    } else if let Some(rest) = low.strip_prefix("layout").filter(|r| is_ascii_digits(r)) {
        Some(py_int_digits(rest)?)
    } else {
        None
    };
    let mut candidates: Vec<&LayoutDef> = match lid {
        Some(l) => l.to_u64().and_then(|id| db.layouts.get(&id)).into_iter().collect(),
        None => db.layouts.values().filter(|l| unicode::casefold(&l.name) == low).collect(),
    };
    if let Some(rf) = &realm_filter {
        candidates.retain(|l| &l.realm_id == rf);
    }
    if candidates.is_empty() {
        return Err(syn(format!("Layout/Tabelle {} nicht gefunden", repr(&source.join(".")))));
    }
    if candidates.len() > 1 {
        return Err(syn(format!("Layoutname {} ist nicht eindeutig; realm.layout verwenden", repr(layout_name))));
    }
    let lay = candidates[0];
    if lay.kind != LayoutKind::Classic {
        return Err(syn("NHySQL V0.27 SELECT bildet Tabellen zunaechst auf CLASSIC-Layouts ab"));
    }
    Ok(lay)
}

/// `_sql_layout_fields`: BASE-Namen, dann Extension-Namen (ohne Duplikate).
pub fn layout_fields(db: &Db, lay: &LayoutDef) -> Result<Vec<String>> {
    let info = db.get_layout(None, None, Some(&LayoutRef::Id(lay.layout_id)))?;
    let mut names: Vec<String> = Vec::new();
    for key in ["base", "extensions"] {
        if let Some(Val::List(items)) = info.get(key) {
            for it in items {
                let name = match it.get("name") {
                    Some(n) if n.is_truthy() => n.py_str(),
                    _ => String::new(),
                };
                if !name.is_empty() && !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    Ok(names)
}

/// `_sql_field_map`: casefold → Name; kollidierende Schreibweisen entfallen.
fn field_map(fields: &[String]) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let mut collisions = Vec::new();
    for name in fields {
        let k = unicode::casefold(name);
        match out.get(&k) {
            Some(existing) if existing != name => collisions.push(k),
            Some(_) => {}
            None => {
                out.insert(k, name.clone());
            }
        }
    }
    for k in collisions {
        out.remove(&k);
    }
    out
}

fn resolve_field(raw: &str, stmt: &SelectStatement, fmap: &HashMap<String, String>) -> Result<String> {
    let name = unqualify(raw, stmt)?;
    fmap.get(&unicode::casefold(&name))
        .cloned()
        .ok_or_else(|| syn(format!("Feld {} existiert im CLASSIC-Layout nicht", repr(&name))))
}

fn resolve_expr(e: &Expr, stmt: &SelectStatement, fmap: &HashMap<String, String>) -> Result<Expr> {
    Ok(match e {
        Expr::Field { name, op, value, value2, options } => {
            Expr::Field { name: resolve_field(name, stmt, fmap)?, op: op.clone(), value: value.clone(), value2: value2.clone(), options: options.clone() }
        }
        Expr::Bool { op, children } => {
            Expr::Bool { op: op.clone(), children: children.iter().map(|c| resolve_expr(c, stmt, fmap)).collect::<Result<_>>()? }
        }
        other => other.clone(),
    })
}

fn out_name(item: &SelectItem) -> String {
    item.alias.clone().unwrap_or_else(|| if item.kind == "relevance" { "relevance".into() } else { item.name.clone() })
}

/// `compile_sql(text, parameters=None, *, lane=0)`
pub fn compile_sql(db: &Db, text: &str, params: &Params, lane: Lane) -> Result<CompiledQuery> {
    let stmt = parse_sql(text, params)?;
    let lay = resolve_source(db, &stmt.source)?;
    let fields = layout_fields(db, lay)?;
    let fmap = field_map(&fields);
    let expr = resolve_expr(&stmt.where_, &stmt, &fmap)?;

    let mut resolved: Vec<SelectItem> = Vec::new();
    for item in &stmt.select_items {
        match item.kind.as_str() {
            "star" => {
                if !item.name.is_empty() {
                    let q = item.name.strip_suffix(".*").unwrap_or(&item.name);
                    if !accepted_qualifier(&stmt, q) {
                        return Err(syn(format!("unbekannter Tabellenalias/Qualifier {} fuer *", repr(q))));
                    }
                }
                resolved.extend(fields.iter().map(|f| SelectItem::new("field", f, "", None)));
            }
            "field" => resolved.push(SelectItem::new("field", &resolve_field(&item.name, &stmt, &fmap)?, "", item.alias.clone())),
            "relevance" => resolved.push(SelectItem::new("relevance", "relevance", "", item.alias.clone())),
            "aggregate" => {
                let name = if item.name == "*" { "*".to_string() } else { resolve_field(&item.name, &stmt, &fmap)? };
                resolved.push(SelectItem::new("aggregate", &name, &item.op, item.alias.clone()));
            }
            other => return Err(syn(format!("unbekanntes SELECT-Element {}", repr(other)))),
        }
    }
    if resolved.is_empty() {
        return Err(syn("SELECT-Liste ist leer"));
    }
    let mut aliases: Vec<(String, SelectItem)> = Vec::new();
    for item in &resolved {
        let on = out_name(item);
        let key = unicode::casefold(&on);
        if aliases.iter().any(|(k, _)| *k == key) {
            return Err(syn(format!("doppelter Ausgabename/Alias {}", repr(&on))));
        }
        aliases.push((key, item.clone()));
    }
    let alias_of = |k: &str| aliases.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());

    let mut group_by: Vec<String> = Vec::new();
    for raw in &stmt.group_by {
        let bare = unqualify(raw, &stmt)?;
        let g = match alias_of(&unicode::casefold(&bare)) {
            Some(it) if it.kind == "field" => it.name,
            _ => resolve_field(raw, &stmt, &fmap)?,
        };
        if !group_by.contains(&g) {
            group_by.push(g);
        }
    }
    let has_agg = resolved.iter().any(|x| x.kind == "aggregate");
    if has_agg {
        if resolved.iter().any(|x| x.kind == "relevance") {
            return Err(syn("relevance kann nicht mit SQL-Aggregaten gemischt werden"));
        }
        for item in &resolved {
            if item.kind == "field" && !group_by.contains(&item.name) {
                return Err(syn(format!("nicht aggregiertes Feld {} muss in GROUP BY stehen", repr(&item.name))));
            }
        }
    } else if !group_by.is_empty() {
        return Err(syn("GROUP BY braucht in NHySQL V0.27 mindestens ein Aggregat im SELECT"));
    }
    let mut aggregate = Val::map();
    if has_agg {
        for item in resolved.iter().filter(|x| x.kind == "aggregate") {
            let alias = item.alias.clone().unwrap_or_else(|| format!("{}_{}", item.op, item.name));
            aggregate.set(alias, vmap! {"op" => item.op.as_str(), "field" => item.name.as_str()});
        }
    }
    let mut order_by: Vec<(String, String)> = Vec::new();
    for raw in &stmt.order_by {
        let bare = unqualify(&raw.name, &stmt)?;
        let key = match alias_of(&unicode::casefold(&bare)) {
            Some(it) => match it.kind.as_str() {
                "aggregate" => it.alias.clone().unwrap_or_default(),
                "relevance" => "relevance".into(),
                _ => it.name.clone(),
            },
            None if unicode::casefold(&bare) == "relevance" => "relevance".into(),
            None => resolve_field(&raw.name, &stmt, &fmap)?,
        };
        order_by.push((key, raw.direction.clone()));
    }
    let ranked = resolved.iter().any(|x| x.kind == "relevance") || order_by.iter().any(|(k, _)| k == "relevance");
    if ranked {
        if has_agg {
            return Err(syn("FULLTEXT relevance und Aggregate koennen nicht im selben SELECT gerankt werden"));
        }
        if order_by.iter().any(|(k, _)| k != "relevance") {
            return Err(syn("ranked FULLTEXT unterstuetzt in V0.27 nur ORDER BY relevance DESC"));
        }
        if order_by.iter().any(|(_, d)| d != "desc") {
            return Err(syn("relevance kann nur DESC sortiert werden"));
        }
        order_by.clear();
    }
    let mut select: Vec<String> = resolved.iter().filter(|x| x.kind == "field").map(|x| x.name.clone()).collect();
    if !has_agg && !ranked {
        for (k, _) in &order_by {
            if k != "relevance" && !select.contains(k) {
                select.push(k.clone());
            }
        }
    }
    let return_mode = if has_agg { "aggregate" } else if ranked { "ranked" } else { "fields" };
    Ok(CompiledQuery {
        layout_id: lay.layout_id,
        layout_name: lay.name.clone(),
        realm: lay.realm_id.clone(),
        lane,
        expr,
        return_mode: return_mode.into(),
        select,
        aggregate,
        group_by,
        order_by,
        limit: stmt.limit.clone(),
        offset: stmt.offset.clone(),
        ranked,
        output: resolved,
        all_fields: fields,
        statement: stmt,
    })
}

/// `_sql_output_columns`
pub fn output_columns(cq: &CompiledQuery) -> Vec<String> {
    cq.output
        .iter()
        .map(|it| match it.kind.as_str() {
            "aggregate" => it.alias.clone().unwrap_or_default(),
            "relevance" => it.alias.clone().unwrap_or_else(|| "relevance".into()),
            _ => it.alias.clone().unwrap_or_else(|| it.name.clone()),
        })
        .collect()
}

fn project_rows(rows: &Val, cq: &CompiledQuery) -> Vec<Val> {
    let Some(rows) = rows.as_list() else { return Vec::new() };
    rows.iter()
        .map(|raw| {
            let mut out = Val::map();
            for it in &cq.output {
                let (source, target) = match it.kind.as_str() {
                    "aggregate" => (it.alias.clone().unwrap_or_default(), it.alias.clone().unwrap_or_default()),
                    "relevance" => ("relevance".to_string(), it.alias.clone().unwrap_or_else(|| "relevance".into())),
                    _ => (it.name.clone(), it.alias.clone().unwrap_or_else(|| it.name.clone())),
                };
                out.set(target, raw.get(&source).cloned().unwrap_or(Val::None));
            }
            out
        })
        .collect()
}

fn opt_big(v: &Option<BigInt>) -> Val {
    v.clone().map(Val::from_bigint).unwrap_or(Val::None)
}

/// `execute_sql(text, parameters=None, *, lane=0)`
pub fn execute_sql(engine: &QueryEngine<'_>, text: &str, params: &Params, lane: Lane) -> Result<Val> {
    let cq = compile_sql(engine.db, text, params, lane)?;
    let columns = output_columns(&cq);
    let source = vmap! {"realm" => &cq.realm, "layout_id" => cq.layout_id as i64, "layout_name" => cq.layout_name.as_str()};
    let realm = Some(cq.realm.to_bigint());
    if cq.statement.explain {
        let plan = engine.explain(&cq.expr, realm, cq.lane, &cq.return_mode)?;
        return Ok(vmap! {
            "query_version" => QUERY_VERSION, "language" => SQL_LANGUAGE_VERSION, "statement" => "EXPLAIN SELECT",
            "source" => source, "columns" => Val::List(Vec::new()), "rows" => Val::List(Vec::new()), "returned" => 0i64,
            "plan" => plan.get("plan").cloned().unwrap_or(Val::None), "optimizer" => plan.get("optimizer").cloned().unwrap_or(Val::None),
        });
    }
    let only_count = cq.output.len() == 1 && cq.output[0].kind == "aggregate" && cq.output[0].op == "count" && cq.output[0].name == "*" && cq.group_by.is_empty();
    let base = ExecOptions { realm, lane: cq.lane, limit: opt_big(&cq.limit), offset: Val::from_bigint(cq.offset.clone()), ..Default::default() };
    let order_val = || Val::List(cq.order_by.iter().map(|(k, d)| Val::List(vec![Val::str(k.as_str()), Val::str(d.as_str())])).collect());
    let select_val = || Val::List(cq.select.iter().map(|s| Val::str(s.as_str())).collect());
    let (engine_res, rows) = if only_count {
        let r = engine.execute(&cq.expr, &ExecOptions { return_mode: "count".into(), ..base })?;
        let alias = cq.output[0].alias.clone().unwrap_or_default();
        let count = r.get("count").cloned().unwrap_or(Val::Int(0));
        let count = Val::from_bigint(query::py_int(&count)?);
        (r, vec![vmap! {alias => count}])
    } else if cq.return_mode == "aggregate" {
        let r = engine.execute(
            &cq.expr,
            &ExecOptions {
                return_mode: "aggregate".into(),
                aggregate: Some(cq.aggregate.clone()),
                group_by: Some(Val::List(cq.group_by.iter().map(|g| Val::str(g.as_str())).collect())),
                order_by: Some(order_val()),
                ..base
            },
        )?;
        let rows = project_rows(r.get("rows").unwrap_or(&Val::None), &cq);
        (r, rows)
    } else if cq.ranked {
        let limit = match &cq.limit {
            Some(l) => Val::from_bigint(l.clone()),
            None => Val::Int(20),
        };
        let r = engine.search(&cq.expr, base.realm.clone(), cq.lane, Some(&select_val()), &limit, &base.offset, false, 1.2, 0.75)?;
        let rows = project_rows(r.get("rows").unwrap_or(&Val::None), &cq);
        (r, rows)
    } else {
        let r = engine.execute(&cq.expr, &ExecOptions { return_mode: "fields".into(), select: Some(select_val()), order_by: Some(order_val()), ..base })?;
        let rows = project_rows(r.get("rows").unwrap_or(&Val::None), &cq);
        (r, rows)
    };
    let matched = match engine_res.get("count") {
        Some(c) => Val::from_bigint(query::py_int(c)?),
        None => Val::Int(rows.len() as i64),
    };
    let mut result = vmap! {
        "query_version" => QUERY_VERSION, "language" => SQL_LANGUAGE_VERSION, "statement" => "SELECT", "source" => source,
        "columns" => Val::List(columns.into_iter().map(Val::Str).collect()), "returned" => rows.len() as i64,
        "rows" => Val::List(rows), "matched" => matched,
        "optimizer" => engine_res.get("optimizer").cloned().unwrap_or_else(|| Val::str(query::OPTIMIZER)),
    };
    // Schlüsselreihenfolge wie Python: columns, rows, returned, matched, optimizer
    let returned = result.remove("returned").unwrap();
    let matched = result.remove("matched").unwrap();
    let optimizer = result.remove("optimizer").unwrap();
    result.set("returned", returned);
    result.set("matched", matched);
    result.set("optimizer", optimizer);
    for key in ["fastpath", "index_id", "index_name", "query_terms", "k1", "b"] {
        if let Some(v) = engine_res.get(key) {
            result.set(key, v.clone());
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexer_and_errors() {
        let t = scan("SELECT a, \"b c\" FROM r WHERE x <> 'it''s' AND y >= -1.5e3 -- c\n AND z = ?").unwrap();
        assert_eq!(t.last().unwrap().kind, TokKind::Eof);
        assert!(t.iter().any(|x| x.kind == TokKind::Ident && x.value == Val::str("b c")));
        assert!(t.iter().any(|x| x.kind == TokKind::Str && x.value == Val::str("it's")));
        assert!(t.iter().any(|x| x.kind == TokKind::Number && x.value == Val::Float(-1500.0)));
        assert!(t.iter().any(|x| x.kind == TokKind::Param && x.value == Val::Int(0)));
        assert_eq!(scan("x = ٣").unwrap()[2].value, Val::Int(3));
        assert!(matches!(scan("x = ①"), Err(Error::SqlSyntax(m)) if m == "ungueltige Zahl '①'"));
        assert_eq!(py_float_literal("1."), Some(1.0));
        assert_eq!(py_float_literal("1.e2"), Some(100.0));
        assert_eq!(py_float_literal("1e"), None);
        let e = parse_sql("SELECT id\nFROM records WHERE", &Params::None).unwrap_err().to_string();
        assert_eq!(e, "Identifier erwartet (Zeile 2, Spalte 19, bei '')");
        let s = parse_sql("SELECT COUNT(*), r.id x FROM main.records AS r WHERE NOT r.a = 1 OR @u BETWEEN 1 AND 3 LIMIT 5", &Params::None).unwrap();
        assert_eq!(s.select_items[0].alias.as_deref(), Some("count_all"));
        assert_eq!(s.source, vec!["main".to_string(), "records".to_string()]);
        assert_eq!(s.limit, Some(BigInt::from(5)));
    }
}
