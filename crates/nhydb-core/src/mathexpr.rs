//! MATH_MAP expression language (`MathExpression`, `_Validator`,
//! `_SafeEvaluator`, `MathMap`, `MathMapLayoutSpec`, `dry_run`).
//!
//! Expressions follow Python's expression grammar (after `^ → **`) and the
//! syntax tree is checked against a whitelist. This parser reproduces the
//! Python grammar for expressions (precedence, right associativity of `**`,
//! `-2**2 = -4`, comparison chains, literals `0x/0o/0b/_`) and reports
//! disallowed constructs with the Python AST node names (`Attribute`,
//! `Subscript`, `Div`, …), so validation errors read the same way.
//! Arithmetic on arbitrarily large integers with Python semantics
//! (floor division/modulo, `**` with a negative exponent ⇒ error "must
//! yield an integer", `bool` forbidden in arithmetic).
//!
//! Safety limit: no intermediate result may exceed `max_bits` bits
//! (default 2^20). Expensive operations (`*`, `**`, `lcm`) are estimated
//! **before** they are computed.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use num_bigint::{BigInt, Sign};
use num_integer::Integer;
use num_traits::{One, Signed, ToPrimitive, Zero};

use crate::error::{Error, Result};
use crate::unicode;
use crate::val::Val;
use crate::vmap;

pub const NL: i64 = 0;
pub const NR: i64 = 1;
pub const MAP_CONTRACT: &str = "NHYDB_MATH_MAP_V015";

static MAX_BITS: AtomicU64 = AtomicU64::new(crate::config::DEFAULT_MATH_MAX_INTERMEDIATE_BITS);

/// Process-wide default limit for intermediate results (bits).
pub fn default_max_bits() -> u64 {
    MAX_BITS.load(Ordering::Relaxed)
}

pub fn set_default_max_bits(bits: u64) {
    MAX_BITS.store(bits.max(1), Ordering::Relaxed);
}

fn syn(m: impl Into<String>) -> Error {
    Error::MathSyntax(m.into())
}

fn dom(m: impl Into<String>) -> Error {
    Error::MathDomain(m.into())
}

const SAFE_FUNCTIONS: [&str; 6] = ["abs", "gcd", "lcm", "isqrt", "min", "max"];

// ----------------------------------------------------------------------
// Syntax tree (Python node types)
// ----------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Int(BigInt),
    Bool(bool),
    /// float, complex, str, bytes, None, Ellipsis – allowed in the tree, but
    /// not an integer when evaluated.
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinK {
    Add,
    Sub,
    Mult,
    MatMult,
    Div,
    Mod,
    Pow,
    LShift,
    RShift,
    BitOr,
    BitXor,
    BitAnd,
    FloorDiv,
}

impl BinK {
    fn name(self) -> &'static str {
        match self {
            BinK::Add => "Add",
            BinK::Sub => "Sub",
            BinK::Mult => "Mult",
            BinK::MatMult => "MatMult",
            BinK::Div => "Div",
            BinK::Mod => "Mod",
            BinK::Pow => "Pow",
            BinK::LShift => "LShift",
            BinK::RShift => "RShift",
            BinK::BitOr => "BitOr",
            BinK::BitXor => "BitXor",
            BinK::BitAnd => "BitAnd",
            BinK::FloorDiv => "FloorDiv",
        }
    }
    fn allowed(self) -> bool {
        matches!(self, BinK::Add | BinK::Sub | BinK::Mult | BinK::FloorDiv | BinK::Mod | BinK::Pow)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnK {
    UAdd,
    USub,
    Not,
    Invert,
}

impl UnK {
    fn name(self) -> &'static str {
        match self {
            UnK::UAdd => "UAdd",
            UnK::USub => "USub",
            UnK::Not => "Not",
            UnK::Invert => "Invert",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpK {
    Eq,
    NotEq,
    Lt,
    LtE,
    Gt,
    GtE,
    Is,
    IsNot,
    In,
    NotIn,
}

impl CmpK {
    fn name(self) -> &'static str {
        match self {
            CmpK::Eq => "Eq",
            CmpK::NotEq => "NotEq",
            CmpK::Lt => "Lt",
            CmpK::LtE => "LtE",
            CmpK::Gt => "Gt",
            CmpK::GtE => "GtE",
            CmpK::Is => "Is",
            CmpK::IsNot => "IsNot",
            CmpK::In => "In",
            CmpK::NotIn => "NotIn",
        }
    }
    fn allowed(self) -> bool {
        !matches!(self, CmpK::Is | CmpK::IsNot | CmpK::In | CmpK::NotIn)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Const(Const),
    Name(String),
    Bin(Box<Node>, BinK, Box<Node>),
    /// Left-associative chain of one precedence level (`a + b - c`), kept
    /// n-ary so that long chains do not create deep trees (F017).
    Chain(Box<Node>, Vec<(BinK, Node)>),
    Un(UnK, Box<Node>),
    And(Vec<Node>),
    Or(Vec<Node>),
    Cmp(Box<Node>, Vec<(CmpK, Node)>),
    Call { func: Box<Node>, args: Vec<Node>, keywords: usize },
    /// Disallowed node with its Python class name.
    Other(&'static str),
}

// ----------------------------------------------------------------------
// Lexer (Python tokens for expressions)
// ----------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Name(String),
    Int(BigInt),
    /// float/complex
    NumOther,
    Str { fstring: bool },
    Op(&'static str),
    End,
}

const OPS: [&str; 46] = [
    "**=", "//=", ">>=", "<<=", "...", "->", "**", "//", "<<", ">>", "<=", ">=", "==", "!=", ":=", "+=", "-=", "*=", "/=", "%=",
    "&=", "|=", "^=", "@=", "+", "-", "*", "/", "%", "@", "&", "|", "^", "~", "<", ">", "(", ")", "[", "]", "{", "}", ",", ":",
    ".", "=",
];

const KEYWORDS: [&str; 35] = [
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or",
    "pass", "raise", "return", "try", "while", "with", "yield",
];

/// Bracket nesting limit (CPython tokenizer `MAXLEVEL`).
const MAX_PAREN_DEPTH: i32 = 200;
/// Limit for recursive grammar levels (brackets, unary operators, `not`,
/// right-associative `**`), like CPython's 200 levels (F017).
const MAX_NESTING: usize = 200;
/// Limit for the depth of the syntax tree; guarantees bounded recursion in
/// validation, evaluation, the fast path and `Drop` (F017).
const MAX_AST_DEPTH: usize = 1000;

fn too_deep() -> Error {
    syn(format!("expression too deeply nested (more than {MAX_NESTING} levels)"))
}

fn invalid() -> Error {
    syn("invalid syntax (<unknown>, line 1)")
}

fn lex(src: &str) -> Result<Vec<Tok>> {
    let cs: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    let mut depth = 0i32;
    while i < cs.len() {
        let c = cs[i];
        if c == '\\' && cs.get(i + 1) == Some(&'\n') {
            i += 2;
            continue;
        }
        if c == ' ' || c == '\t' || c == '\x0c' || ((c == '\n' || c == '\r') && depth > 0) {
            i += 1;
            continue;
        }
        if c == '\n' || c == '\r' {
            // end of line outside brackets terminates the expression; only
            // blank or comment lines may follow (as in `ast.parse`, F042)
            let rest: String = cs[i..].iter().collect();
            if rest.lines().all(|l| {
                let t = l.trim_start_matches([' ', '\t', '\x0c']);
                t.is_empty() || t.starts_with('#') || t.chars().all(|x| x.is_whitespace())
            }) {
                break;
            }
            return Err(invalid());
        }
        if c == '#' {
            while i < cs.len() && cs[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // numbers
        if c.is_ascii_digit() || (c == '.' && cs.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let start = i;
            let mut is_int = true;
            let radix_prefix = c == '0' && cs.get(i + 1).map(|x| "xXoObB".contains(*x)).unwrap_or(false);
            if radix_prefix {
                let radix = match cs[i + 1].to_ascii_lowercase() {
                    'x' => 16,
                    'o' => 8,
                    _ => 2,
                };
                i += 2;
                let ds = i;
                while i < cs.len() && (cs[i].is_digit(radix) || cs[i] == '_') {
                    i += 1;
                }
                let digits: String = cs[ds..i].iter().collect();
                // `0x_ff` is valid Python (one underscore after the prefix, F042)
                if digits.trim_start_matches('_').is_empty() || digits.ends_with('_') || digits.contains("__") {
                    return Err(invalid());
                }
                if !number_end_ok(&cs, i) {
                    return Err(invalid());
                }
                let clean = digits.replace('_', "");
                let v = BigInt::parse_bytes(clean.as_bytes(), radix).ok_or_else(invalid)?;
                out.push(Tok::Int(v));
                continue;
            }
            while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '_') {
                i += 1;
            }
            if i < cs.len() && cs[i] == '.' {
                is_int = false;
                i += 1;
                while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '_') {
                    i += 1;
                }
            }
            if i < cs.len() && (cs[i] == 'e' || cs[i] == 'E') {
                let save = i;
                i += 1;
                if i < cs.len() && (cs[i] == '+' || cs[i] == '-') {
                    i += 1;
                }
                if i < cs.len() && cs[i].is_ascii_digit() {
                    is_int = false;
                    while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '_') {
                        i += 1;
                    }
                } else {
                    i = save;
                }
            }
            if i < cs.len() && (cs[i] == 'j' || cs[i] == 'J') {
                i += 1;
                is_int = false;
            }
            if !number_end_ok(&cs, i) {
                return Err(invalid());
            }
            let text: String = cs[start..i].iter().collect();
            if text.contains("__") || text.ends_with('_') || text.contains("_.") || text.contains("._") {
                return Err(invalid());
            }
            if is_int {
                let clean = text.replace('_', "");
                if clean.len() > 1 && clean.starts_with('0') && clean.chars().any(|d| d != '0') {
                    return Err(invalid());
                }
                out.push(Tok::Int(clean.parse::<BigInt>().map_err(|_| invalid())?));
            } else {
                out.push(Tok::NumOther);
            }
            continue;
        }
        // names and string prefixes
        if c == '_' || unicode::is_identifier(&c.to_string()) {
            let start = i;
            i += 1;
            while i < cs.len() && (cs[i] == '_' || unicode::is_identifier(&format!("a{}", cs[i]))) {
                i += 1;
            }
            let name: String = cs[start..i].iter().collect();
            let lower = name.to_ascii_lowercase();
            if i < cs.len() && (cs[i] == '\'' || cs[i] == '"') && name.len() <= 2 && lower.chars().all(|x| "rbuf".contains(x)) {
                let fstring = lower.contains('f');
                i = skip_string(&cs, i)?;
                out.push(Tok::Str { fstring });
                continue;
            }
            if unicode::identifier_needs_nfkc(&name) {
                return Err(syn(format!(
                    "identifier {} is not NFKC-normalized (Python normalizes identifiers; please use the normalized form)",
                    Val::str(name.as_str()).py_repr()
                )));
            }
            out.push(Tok::Name(name));
            continue;
        }
        if c == '\'' || c == '"' {
            i = skip_string(&cs, i)?;
            out.push(Tok::Str { fstring: false });
            continue;
        }
        let rest: String = cs[i..(i + 3).min(cs.len())].iter().collect();
        match OPS.iter().find(|op| rest.starts_with(**op)) {
            Some(op) => {
                if matches!(*op, "(" | "[" | "{") {
                    depth += 1;
                    if depth > MAX_PAREN_DEPTH {
                        // same limit and message as the CPython tokenizer (MAXLEVEL)
                        return Err(syn("too many nested parentheses (<unknown>, line 1)"));
                    }
                }
                if matches!(*op, ")" | "]" | "}") {
                    depth -= 1;
                }
                out.push(Tok::Op(op));
                i += op.chars().count();
            }
            None => return Err(invalid()),
        }
    }
    out.push(Tok::End);
    Ok(out)
}

/// CPython's `verify_end_of_number`: a number may be directly followed by
/// the keywords `and else for if in is not or` (e.g. `1if x else 2`), but
/// not by other identifier characters (F042).
fn number_end_ok(cs: &[char], i: usize) -> bool {
    let Some(&c) = cs.get(i) else { return true };
    if !unicode::is_word(c) {
        return true;
    }
    ["and", "else", "for", "if", "in", "is", "not", "or"].iter().any(|kw| {
        let n = kw.len();
        cs.len() >= i + n
            && cs[i..i + n].iter().copied().eq(kw.chars())
            && cs.get(i + n).is_none_or(|x| !unicode::is_word(*x))
    })
}

fn skip_string(cs: &[char], mut i: usize) -> Result<usize> {
    let q = cs[i];
    let triple = cs.get(i + 1) == Some(&q) && cs.get(i + 2) == Some(&q);
    i += if triple { 3 } else { 1 };
    loop {
        if i >= cs.len() {
            return Err(invalid());
        }
        let c = cs[i];
        if c == '\\' {
            i += 2;
            continue;
        }
        if triple {
            if c == q && cs.get(i + 1) == Some(&q) && cs.get(i + 2) == Some(&q) {
                return Ok(i + 3);
            }
        } else {
            if c == '\n' {
                return Err(invalid());
            }
            if c == q {
                return Ok(i + 1);
            }
        }
        i += 1;
    }
}

// ----------------------------------------------------------------------
// Parser (Python expression grammar)
// ----------------------------------------------------------------------

struct Parser {
    toks: Vec<Tok>,
    i: usize,
    /// Current recursion level (see `MAX_NESTING`).
    nest: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.i]
    }
    fn peek2(&self) -> &Tok {
        self.toks.get(self.i + 1).unwrap_or(&Tok::End)
    }
    fn is_op(&self, op: &str) -> bool {
        matches!(self.peek(), Tok::Op(o) if *o == op)
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Name(n) if n == kw)
    }
    fn eat_op(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect_op(&mut self, op: &str) -> Result<()> {
        if self.eat_op(op) { Ok(()) } else { Err(invalid()) }
    }

    /// Runs `f` one recursion level deeper; fails cleanly beyond `MAX_NESTING`.
    /// The top-level expression is level 1, so 200 nested brackets are allowed.
    fn nested<T>(&mut self, f: impl FnOnce(&mut Parser) -> Result<T>) -> Result<T> {
        if self.nest > MAX_NESTING {
            return Err(too_deep());
        }
        self.nest += 1;
        let r = f(self);
        self.nest -= 1;
        r
    }

    /// expression: lambda | disjunction ['if' disjunction 'else' expression]
    fn expression(&mut self) -> Result<Node> {
        self.nested(Parser::expression_inner)
    }

    fn expression_inner(&mut self) -> Result<Node> {
        if self.eat_kw("lambda") {
            while !self.is_op(":") {
                if matches!(self.peek(), Tok::End) {
                    return Err(invalid());
                }
                self.i += 1;
            }
            self.i += 1;
            self.expression()?;
            return Ok(Node::Other("Lambda"));
        }
        let d = self.disjunction()?;
        if self.eat_kw("if") {
            self.disjunction()?;
            if !self.eat_kw("else") {
                return Err(invalid());
            }
            self.expression()?;
            return Ok(Node::Other("IfExp"));
        }
        Ok(d)
    }

    fn disjunction(&mut self) -> Result<Node> {
        let first = self.conjunction()?;
        if !self.is_kw("or") {
            return Ok(first);
        }
        let mut v = vec![first];
        while self.eat_kw("or") {
            v.push(self.conjunction()?);
        }
        Ok(Node::Or(v))
    }

    fn conjunction(&mut self) -> Result<Node> {
        let first = self.inversion()?;
        if !self.is_kw("and") {
            return Ok(first);
        }
        let mut v = vec![first];
        while self.eat_kw("and") {
            v.push(self.inversion()?);
        }
        Ok(Node::And(v))
    }

    fn inversion(&mut self) -> Result<Node> {
        if self.eat_kw("not") {
            return Ok(Node::Un(UnK::Not, Box::new(self.nested(Parser::inversion)?)));
        }
        self.comparison()
    }

    fn cmp_op(&mut self) -> Option<CmpK> {
        let k = match self.peek() {
            Tok::Op("==") => CmpK::Eq,
            Tok::Op("!=") => CmpK::NotEq,
            Tok::Op("<") => CmpK::Lt,
            Tok::Op("<=") => CmpK::LtE,
            Tok::Op(">") => CmpK::Gt,
            Tok::Op(">=") => CmpK::GtE,
            Tok::Name(n) if n == "in" => CmpK::In,
            Tok::Name(n) if n == "is" => {
                if matches!(self.peek2(), Tok::Name(m) if m == "not") {
                    self.i += 1;
                    CmpK::IsNot
                } else {
                    CmpK::Is
                }
            }
            Tok::Name(n) if n == "not" && matches!(self.peek2(), Tok::Name(m) if m == "in") => {
                self.i += 1;
                CmpK::NotIn
            }
            _ => return None,
        };
        self.i += 1;
        Some(k)
    }

    fn comparison(&mut self) -> Result<Node> {
        let left = self.bitor()?;
        let mut ops = Vec::new();
        while let Some(k) = self.cmp_op() {
            ops.push((k, self.bitor()?));
        }
        if ops.is_empty() { Ok(left) } else { Ok(Node::Cmp(Box::new(left), ops)) }
    }

    /// One left-associative precedence level, collected n-ary (iterative,
    /// no tree depth per operator).
    fn bin_level(&mut self, ops: &[(&str, BinK)], next: fn(&mut Parser) -> Result<Node>) -> Result<Node> {
        let first = next(self)?;
        let mut rest: Vec<(BinK, Node)> = Vec::new();
        'outer: loop {
            for (o, k) in ops {
                if self.is_op(o) {
                    self.i += 1;
                    rest.push((*k, next(self)?));
                    continue 'outer;
                }
            }
            break;
        }
        Ok(if rest.is_empty() { first } else { Node::Chain(Box::new(first), rest) })
    }

    fn bitor(&mut self) -> Result<Node> {
        self.bin_level(&[("|", BinK::BitOr)], Parser::bitxor)
    }
    fn bitxor(&mut self) -> Result<Node> {
        self.bin_level(&[("^", BinK::BitXor)], Parser::bitand)
    }
    fn bitand(&mut self) -> Result<Node> {
        self.bin_level(&[("&", BinK::BitAnd)], Parser::shift)
    }
    fn shift(&mut self) -> Result<Node> {
        self.bin_level(&[("<<", BinK::LShift), (">>", BinK::RShift)], Parser::sum)
    }
    fn sum(&mut self) -> Result<Node> {
        self.bin_level(&[("+", BinK::Add), ("-", BinK::Sub)], Parser::term)
    }
    fn term(&mut self) -> Result<Node> {
        self.bin_level(
            &[("*", BinK::Mult), ("//", BinK::FloorDiv), ("/", BinK::Div), ("%", BinK::Mod), ("@", BinK::MatMult)],
            Parser::factor,
        )
    }

    fn factor(&mut self) -> Result<Node> {
        let k = if self.eat_op("+") {
            UnK::UAdd
        } else if self.eat_op("-") {
            UnK::USub
        } else if self.eat_op("~") {
            UnK::Invert
        } else {
            return self.power();
        };
        Ok(Node::Un(k, Box::new(self.nested(Parser::factor)?)))
    }

    fn power(&mut self) -> Result<Node> {
        if self.eat_kw("await") {
            self.primary()?;
            return Ok(Node::Other("Await"));
        }
        let base = self.primary()?;
        if self.eat_op("**") {
            let exp = self.nested(Parser::factor)?;
            return Ok(Node::Bin(Box::new(base), BinK::Pow, Box::new(exp)));
        }
        Ok(base)
    }

    fn primary(&mut self) -> Result<Node> {
        let mut node = self.atom()?;
        loop {
            if self.eat_op(".") {
                match self.peek() {
                    Tok::Name(_) => self.i += 1,
                    _ => return Err(invalid()),
                }
                node = Node::Other("Attribute");
            } else if self.eat_op("(") {
                let (args, keywords) = self.call_args()?;
                node = Node::Call { func: Box::new(node), args, keywords };
            } else if self.eat_op("[") {
                self.skip_until_close("]")?;
                node = Node::Other("Subscript");
            } else {
                return Ok(node);
            }
        }
    }

    fn call_args(&mut self) -> Result<(Vec<Node>, usize)> {
        let mut args = Vec::new();
        let mut keywords = 0;
        while !self.eat_op(")") {
            if self.eat_op("*") {
                self.expression()?;
                args.push(Node::Other("Starred"));
            } else if self.eat_op("**") {
                self.expression()?;
                keywords += 1;
            } else if matches!(self.peek(), Tok::Name(_)) && matches!(self.peek2(), Tok::Op("=")) {
                self.i += 2;
                self.expression()?;
                keywords += 1;
            } else {
                let e = self.expression()?;
                if self.is_kw("for") || self.is_kw("async") {
                    self.skip_until_close(")")?;
                    args.push(Node::Other("GeneratorExp"));
                    return Ok((args, keywords));
                }
                args.push(e);
            }
            if !self.eat_op(",") {
                self.expect_op(")")?;
                break;
            }
        }
        Ok((args, keywords))
    }

    /// Skips up to and including the matching closing bracket.
    fn skip_until_close(&mut self, close: &str) -> Result<()> {
        let mut depth = 1;
        loop {
            match self.peek() {
                Tok::End => return Err(invalid()),
                Tok::Op(o) if matches!(*o, "(" | "[" | "{") => depth += 1,
                Tok::Op(o) if matches!(*o, ")" | "]" | "}") => {
                    depth -= 1;
                    if depth == 0 {
                        if *o != close {
                            return Err(invalid());
                        }
                        self.i += 1;
                        return Ok(());
                    }
                }
                _ => {}
            }
            self.i += 1;
        }
    }

    fn atom(&mut self) -> Result<Node> {
        let t = self.peek().clone();
        self.i += 1;
        match t {
            Tok::Int(v) => Ok(Node::Const(Const::Int(v))),
            Tok::NumOther => Ok(Node::Const(Const::Other)),
            Tok::Str { fstring } => {
                let mut f = fstring;
                while let Tok::Str { fstring } = self.peek() {
                    f |= *fstring;
                    self.i += 1;
                }
                Ok(if f { Node::Other("JoinedStr") } else { Node::Const(Const::Other) })
            }
            Tok::Name(n) => match n.as_str() {
                "True" => Ok(Node::Const(Const::Bool(true))),
                "False" => Ok(Node::Const(Const::Bool(false))),
                "None" => Ok(Node::Const(Const::Other)),
                k if KEYWORDS.contains(&k) => Err(invalid()),
                _ => Ok(Node::Name(n)),
            },
            Tok::Op("...") => Ok(Node::Const(Const::Other)),
            Tok::Op("(") => {
                if self.eat_op(")") {
                    return Ok(Node::Other("Tuple"));
                }
                if self.is_kw("yield") {
                    self.skip_until_close(")")?;
                    return Ok(Node::Other("Yield"));
                }
                let first = if self.is_op("*") {
                    self.i += 1;
                    self.bitor()?;
                    Node::Other("Starred")
                } else {
                    self.expression()?
                };
                if self.eat_op(")") {
                    return Ok(first);
                }
                if self.is_kw("for") || self.is_kw("async") {
                    self.skip_until_close(")")?;
                    return Ok(Node::Other("GeneratorExp"));
                }
                if self.is_op(",") {
                    self.skip_until_close(")")?;
                    return Ok(Node::Other("Tuple"));
                }
                Err(invalid())
            }
            Tok::Op("[") => {
                let comp = self.scan_comprehension("]")?;
                Ok(Node::Other(if comp { "ListComp" } else { "List" }))
            }
            Tok::Op("{") => {
                let (comp, is_dict) = self.scan_brace()?;
                Ok(Node::Other(match (comp, is_dict) {
                    (true, true) => "DictComp",
                    (true, false) => "SetComp",
                    (false, true) => "Dict",
                    (false, false) => "Set",
                }))
            }
            _ => Err(invalid()),
        }
    }

    fn scan_comprehension(&mut self, close: &str) -> Result<bool> {
        let start = self.i;
        let mut depth = 1;
        let mut comp = false;
        while depth > 0 {
            match self.peek() {
                Tok::End => return Err(invalid()),
                Tok::Op(o) if matches!(*o, "(" | "[" | "{") => depth += 1,
                Tok::Op(o) if matches!(*o, ")" | "]" | "}") => depth -= 1,
                Tok::Name(n) if depth == 1 && n == "for" => comp = true,
                _ => {}
            }
            self.i += 1;
        }
        if !matches!(&self.toks[self.i - 1], Tok::Op(o) if *o == close) {
            self.i = start;
            return Err(invalid());
        }
        Ok(comp)
    }

    fn scan_brace(&mut self) -> Result<(bool, bool)> {
        if self.eat_op("}") {
            return Ok((false, true));
        }
        let mut depth = 1;
        let mut comp = false;
        let mut is_dict = false;
        while depth > 0 {
            match self.peek() {
                Tok::End => return Err(invalid()),
                Tok::Op(o) if matches!(*o, "(" | "[" | "{") => depth += 1,
                Tok::Op(o) if matches!(*o, ")" | "]" | "}") => depth -= 1,
                Tok::Op(":") if depth == 1 => is_dict = true,
                Tok::Op("**") if depth == 1 => is_dict = true,
                Tok::Name(n) if depth == 1 && n == "for" => comp = true,
                _ => {}
            }
            self.i += 1;
        }
        Ok((comp, is_dict))
    }
}

fn parse(source: &str) -> Result<Node> {
    let toks = lex(source)?;
    let mut p = Parser { toks, i: 0, nest: 0 };
    let node = p.expression()?;
    if !matches!(p.peek(), Tok::End) {
        if p.is_op(",") {
            // top-level tuple: `1, 2`
            return Ok(Node::Other("Tuple"));
        }
        return Err(invalid());
    }
    if ast_depth(&node) > MAX_AST_DEPTH {
        return Err(syn(format!("expression too complex (syntax tree deeper than {MAX_AST_DEPTH} levels)")));
    }
    Ok(node)
}

/// Depth of the syntax tree, computed iteratively.
fn ast_depth(root: &Node) -> usize {
    let mut max = 0;
    let mut stack: Vec<(&Node, usize)> = vec![(root, 1)];
    while let Some((n, d)) = stack.pop() {
        max = max.max(d);
        match n {
            Node::Const(_) | Node::Name(_) | Node::Other(_) => {}
            Node::Bin(l, _, r) => {
                stack.push((l, d + 1));
                stack.push((r, d + 1));
            }
            Node::Chain(first, rest) => {
                stack.push((first, d + 1));
                stack.extend(rest.iter().map(|(_, x)| (x, d + 1)));
            }
            Node::Un(_, x) => stack.push((x, d + 1)),
            Node::And(v) | Node::Or(v) => stack.extend(v.iter().map(|x| (x, d + 1))),
            Node::Cmp(l, ops) => {
                stack.push((l, d + 1));
                stack.extend(ops.iter().map(|(_, x)| (x, d + 1)));
            }
            Node::Call { func, args, .. } => {
                stack.push((func, d + 1));
                stack.extend(args.iter().map(|x| (x, d + 1)));
            }
        }
    }
    max
}

// ----------------------------------------------------------------------
// Validation (`_Validator`): same order as ast.NodeVisitor
// ----------------------------------------------------------------------

fn validate(n: &Node, params: &HashSet<String>) -> Result<()> {
    let not_allowed = |name: &str| syn(format!("syntax element {name} is not allowed"));
    match n {
        Node::Const(_) => Ok(()),
        Node::Name(id) => {
            if !params.contains(id) && !SAFE_FUNCTIONS.contains(&id.as_str()) {
                return Err(syn(format!("unknown name {}", Val::str(id.as_str()).py_repr())));
            }
            Ok(())
        }
        Node::Bin(l, k, r) => {
            validate(l, params)?;
            if !k.allowed() {
                return Err(not_allowed(k.name()));
            }
            validate(r, params)
        }
        Node::Chain(first, rest) => {
            // same visiting order as the left-deep BinOp tree of Python's ast
            validate(first, params)?;
            for (k, x) in rest {
                if !k.allowed() {
                    return Err(not_allowed(k.name()));
                }
                validate(x, params)?;
            }
            Ok(())
        }
        Node::Un(k, x) => {
            if *k == UnK::Invert {
                return Err(not_allowed(k.name()));
            }
            validate(x, params)
        }
        Node::And(v) | Node::Or(v) => v.iter().try_for_each(|x| validate(x, params)),
        Node::Cmp(left, ops) => {
            validate(left, params)?;
            for (k, _) in ops {
                if !k.allowed() {
                    return Err(not_allowed(k.name()));
                }
            }
            ops.iter().try_for_each(|(_, x)| validate(x, params))
        }
        Node::Call { func, args, keywords } => {
            match func.as_ref() {
                Node::Name(f) if SAFE_FUNCTIONS.contains(&f.as_str()) => {}
                _ => return Err(syn("only approved mathematical functions are allowed")),
            }
            if *keywords > 0 {
                return Err(syn("keyword arguments are not allowed"));
            }
            args.iter().try_for_each(|x| validate(x, params))
        }
        Node::Other(name) => Err(not_allowed(name)),
    }
}

// ----------------------------------------------------------------------
// Evaluation (`_SafeEvaluator`)
// ----------------------------------------------------------------------

/// Value of a subexpression: Python `int` or `bool`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MV {
    Int(BigInt),
    Bool(bool),
}

impl MV {
    fn truthy(&self) -> bool {
        match self {
            MV::Int(i) => !i.is_zero(),
            MV::Bool(b) => *b,
        }
    }
    fn as_int(&self) -> BigInt {
        match self {
            MV::Int(i) => i.clone(),
            MV::Bool(b) => BigInt::from(*b as i64),
        }
    }
    fn is_bool(&self) -> bool {
        matches!(self, MV::Bool(_))
    }
}

fn bits(x: &BigInt) -> u64 {
    x.bits()
}

struct Eval<'a> {
    vars: &'a HashMap<String, BigInt>,
    max_bits: u64,
}

impl Eval<'_> {
    fn guard(&self, est: u64) -> Result<()> {
        if est > self.max_bits {
            return Err(dom(format!(
                "intermediate result exceeds math_max_intermediate_bits={} (estimated {est} bits)",
                self.max_bits
            )));
        }
        Ok(())
    }

    fn visit(&self, n: &Node) -> Result<MV> {
        match n {
            Node::Const(Const::Int(i)) => Ok(MV::Int(i.clone())),
            Node::Const(Const::Bool(b)) => Ok(MV::Bool(*b)),
            Node::Const(Const::Other) => Err(syn("only integer constants are allowed")),
            Node::Name(id) => match self.vars.get(id) {
                Some(v) => Ok(MV::Int(v.clone())),
                None => Err(dom(format!("variable {} has no value", Val::str(id.as_str()).py_repr()))),
            },
            Node::Bin(l, k, r) => {
                if !k.allowed() {
                    return Err(syn(format!("operator {} is not allowed", k.name())));
                }
                let a = self.visit(l)?;
                let b = self.visit(r)?;
                self.binop(*k, a, b)
            }
            Node::Chain(first, rest) => {
                // the outermost BinOp of the left-deep tree is checked first
                if let Some((k, _)) = rest.iter().rev().find(|(k, _)| !k.allowed()) {
                    return Err(syn(format!("operator {} is not allowed", k.name())));
                }
                let mut acc = self.visit(first)?;
                for (k, x) in rest {
                    let b = self.visit(x)?;
                    acc = self.binop(*k, acc, b)?;
                }
                Ok(acc)
            }
            Node::Un(k, x) => {
                let v = self.visit(x)?;
                Ok(match k {
                    UnK::UAdd => MV::Int(v.as_int()),
                    UnK::USub => MV::Int(-v.as_int()),
                    UnK::Not => MV::Bool(!v.truthy()),
                    UnK::Invert => return Err(syn("unary operator Invert is not allowed")),
                })
            }
            Node::And(v) => {
                for x in v {
                    if !self.visit(x)?.truthy() {
                        return Ok(MV::Bool(false));
                    }
                }
                Ok(MV::Bool(true))
            }
            Node::Or(v) => {
                for x in v {
                    if self.visit(x)?.truthy() {
                        return Ok(MV::Bool(true));
                    }
                }
                Ok(MV::Bool(false))
            }
            Node::Cmp(left, ops) => {
                let mut l = self.visit(left)?.as_int();
                for (k, rn) in ops {
                    let r = self.visit(rn)?.as_int();
                    let ok = match k {
                        CmpK::Eq => l == r,
                        CmpK::NotEq => l != r,
                        CmpK::Lt => l < r,
                        CmpK::LtE => l <= r,
                        CmpK::Gt => l > r,
                        CmpK::GtE => l >= r,
                        other => return Err(syn(format!("comparison {} is not allowed", other.name()))),
                    };
                    if !ok {
                        return Ok(MV::Bool(false));
                    }
                    l = r;
                }
                Ok(MV::Bool(true))
            }
            Node::Call { func, args, .. } => {
                let Node::Name(f) = func.as_ref() else {
                    return Err(syn("only direct mathematical function names are allowed"));
                };
                let mut xs = Vec::with_capacity(args.len());
                for a in args {
                    let v = self.visit(a)?;
                    if v.is_bool() {
                        return Err(dom(format!("function {f} expects integers")));
                    }
                    xs.push(v.as_int());
                }
                let res = self.call(f, &xs)?;
                self.guard(bits(&res))?;
                Ok(MV::Int(res))
            }
            Node::Other(name) => Err(syn(format!("unsupported syntax element: {name}"))),
        }
    }

    /// One arithmetic operation with the intermediate-result guard.
    fn binop(&self, k: BinK, a: MV, b: MV) -> Result<MV> {
        if a.is_bool() || b.is_bool() {
            return Err(dom("boolean values are not allowed in arithmetic"));
        }
        let (a, b) = (a.as_int(), b.as_int());
        let res = match k {
            BinK::Add | BinK::Sub => {
                self.guard(bits(&a).max(bits(&b)) + 1)?;
                if k == BinK::Add { a + b } else { a - b }
            }
            BinK::Mult => {
                self.guard(bits(&a) + bits(&b))?;
                a * b
            }
            BinK::FloorDiv | BinK::Mod => {
                if b.is_zero() {
                    return Err(dom("division by zero"));
                }
                if k == BinK::FloorDiv { a.div_floor(&b) } else { a.mod_floor(&b) }
            }
            BinK::Pow => self.pow(&a, &b)?,
            other => return Err(syn(format!("operator {} is not allowed", other.name()))),
        };
        self.guard(bits(&res))?;
        Ok(MV::Int(res))
    }

    fn pow(&self, a: &BigInt, b: &BigInt) -> Result<BigInt> {
        if b.is_negative() {
            if a.is_zero() {
                return Err(dom("zero to a negative power"));
            }
            // Python yields a float here (also for |a| = 1, where the result is ±1.0)
            return Err(dom("arithmetic expression must yield an integer"));
        }
        if a.is_zero() || a.is_one() || b.is_zero() {
            return Ok(if b.is_zero() { BigInt::one() } else { a.clone() });
        }
        if a == &BigInt::from(-1) {
            return Ok(if b.is_even() { BigInt::one() } else { a.clone() });
        }
        let lg = (a.abs().bits() as f64 - 1.0).max(0.0) + {
            // fractional part of log2|a| from the leading bits
            let top = a.abs() >> (a.abs().bits().saturating_sub(53) as usize);
            let t = top.to_f64().unwrap_or(1.0);
            t.log2() - (top.bits() as f64 - 1.0)
        };
        let e = b.to_f64().unwrap_or(f64::INFINITY);
        let est = (e * lg).floor() + 1.0;
        if !est.is_finite() || est > self.max_bits as f64 {
            return Err(dom(format!(
                "intermediate result exceeds math_max_intermediate_bits={} (estimated {} bits)",
                self.max_bits,
                if est.is_finite() { format!("{est:.0}") } else { "unbounded".into() }
            )));
        }
        let exp = b.to_u32().ok_or_else(|| dom("exponent too large"))?;
        Ok(num_traits::pow(a.clone(), exp as usize))
    }

    fn call(&self, f: &str, xs: &[BigInt]) -> Result<BigInt> {
        let one_arg = |name: &str| -> Result<&BigInt> {
            if xs.len() != 1 {
                let msg = if name == "abs" {
                    format!("abs() takes exactly one argument ({} given)", xs.len())
                } else {
                    format!("math.{name}() takes exactly one argument ({} given)", xs.len())
                };
                return Err(dom(msg));
            }
            Ok(&xs[0])
        };
        Ok(match f {
            "abs" => one_arg("abs")?.abs(),
            "isqrt" => {
                let x = one_arg("isqrt")?;
                if x.is_negative() {
                    return Err(dom("isqrt() argument must be nonnegative"));
                }
                x.sqrt()
            }
            "gcd" => xs.iter().fold(BigInt::zero(), |acc, x| acc.gcd(x)),
            "lcm" => {
                let mut acc = BigInt::one();
                for x in xs {
                    if acc.is_zero() || x.is_zero() {
                        acc = BigInt::zero();
                        continue;
                    }
                    self.guard(bits(&acc) + bits(x))?;
                    acc = acc.lcm(x);
                }
                acc
            }
            "min" | "max" => {
                if xs.is_empty() {
                    return Err(dom(format!("{f}() expects at least one argument")));
                }
                let it = xs.iter();
                if f == "min" { it.min().unwrap().clone() } else { it.max().unwrap().clone() }
            }
            other => return Err(syn(format!("function {} is not allowed", Val::str(other).py_repr()))),
        })
    }
}

// ----------------------------------------------------------------------
// MathExpression
// ----------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct MathExpression {
    pub source: String,
    pub parameters: Vec<String>,
    tree: Node,
    /// Slot-indexed form for the i64 fast path.
    fast: FNode,
}

// ----------------------------------------------------------------------
// i64 fast path
// ----------------------------------------------------------------------
//
// Same semantics as `Eval`, but on i64 with overflow checks and
// variables by index. Every overflow, every error case and every
// unsupported construct yields `None`; the caller then evaluates exactly
// via BigInt (error messages are produced only there).

#[derive(Clone, Debug)]
enum FNode {
    Int(i64),
    /// folded constant outside i64 (the bit limit is checked during evaluation)
    BigConst(BigInt),
    Bool(bool),
    Var(usize),
    Bin(Box<FNode>, BinK, Box<FNode>),
    /// n-ary left-associative chain (see `Node::Chain`)
    Chain(Box<FNode>, Vec<(BinK, FNode)>),
    /// Constant folded at compile time together with the largest bit limit
    /// its computation needed; below that limit the fast path yields `None`
    /// so that the exact path raises the same error (F029).
    Folded(F64v, u64),
    Pos(Box<FNode>),
    Neg(Box<FNode>),
    Not(Box<FNode>),
    And(Vec<FNode>),
    Or(Vec<FNode>),
    Cmp(Box<FNode>, Vec<(CmpK, FNode)>),
    Call(FFun, Vec<FNode>),
    Unsupported,
}

#[derive(Clone, Copy, Debug)]
enum FFun {
    Abs,
    Isqrt,
    Gcd,
    Min,
    Max,
}

/// Hybrid value of the fast path: small integers without allocation.
#[derive(Clone, Debug, PartialEq)]
enum F64v {
    Int(i64),
    Big(BigInt),
    Bool(bool),
}

impl F64v {
    fn big(&self) -> BigInt {
        match self {
            F64v::Int(i) => BigInt::from(*i),
            F64v::Big(b) => b.clone(),
            F64v::Bool(b) => BigInt::from(*b as i64),
        }
    }
    fn norm(b: BigInt) -> F64v {
        match b.to_i64() {
            Some(i) => F64v::Int(i),
            None => F64v::Big(b),
        }
    }
    fn truthy(&self) -> bool {
        match self {
            F64v::Int(i) => *i != 0,
            F64v::Big(b) => !b.is_zero(),
            F64v::Bool(b) => *b,
        }
    }
    fn is_bool(&self) -> bool {
        matches!(self, F64v::Bool(_))
    }
    fn bits(&self) -> u64 {
        match self {
            F64v::Int(i) => 64 - i.unsigned_abs().leading_zeros() as u64,
            F64v::Big(b) => b.bits(),
            F64v::Bool(b) => *b as u64,
        }
    }
    fn cmp(&self, o: &F64v) -> std::cmp::Ordering {
        match (self, o) {
            (F64v::Int(a), F64v::Int(b)) => a.cmp(b),
            _ => self.big().cmp(&o.big()),
        }
    }
}

fn to_fast(n: &Node, params: &[String]) -> FNode {
    let b = |x: &Node| Box::new(to_fast(x, params));
    match n {
        Node::Const(Const::Int(i)) => i.to_i64().map(FNode::Int).unwrap_or_else(|| FNode::BigConst(i.clone())),
        Node::Const(Const::Bool(v)) => FNode::Bool(*v),
        Node::Const(Const::Other) | Node::Other(_) => FNode::Unsupported,
        Node::Name(id) => params.iter().position(|p| p == id).map(FNode::Var).unwrap_or(FNode::Unsupported),
        Node::Bin(l, k, r) => {
            if !fast_op(*k) {
                return FNode::Unsupported;
            }
            fold(FNode::Bin(b(l), *k, b(r)))
        }
        Node::Chain(first, rest) => {
            if rest.iter().any(|(k, _)| !fast_op(*k)) {
                return FNode::Unsupported;
            }
            // fold the leading constant operands (e.g. `10**100 * 3 + x`)
            let mut acc = to_fast(first, params);
            let mut tail = Vec::with_capacity(rest.len());
            let mut it = rest.iter();
            for (k, x) in it.by_ref() {
                let fx = to_fast(x, params);
                if is_const(&acc) && is_const(&fx) {
                    let folded = fold(FNode::Bin(Box::new(acc.clone()), *k, Box::new(fx.clone())));
                    if is_const(&folded) {
                        acc = folded;
                        continue;
                    }
                }
                tail.push((*k, fx));
                break;
            }
            tail.extend(it.map(|(k, x)| (*k, to_fast(x, params))));
            if tail.is_empty() { acc } else { FNode::Chain(Box::new(acc), tail) }
        }
        Node::Un(UnK::UAdd, x) => FNode::Pos(b(x)),
        Node::Un(UnK::USub, x) => FNode::Neg(b(x)),
        Node::Un(UnK::Not, x) => FNode::Not(b(x)),
        Node::Un(UnK::Invert, _) => FNode::Unsupported,
        Node::And(v) => FNode::And(v.iter().map(|x| to_fast(x, params)).collect()),
        Node::Or(v) => FNode::Or(v.iter().map(|x| to_fast(x, params)).collect()),
        Node::Cmp(l, ops) => {
            if ops.iter().any(|(k, _)| !matches!(k, CmpK::Eq | CmpK::NotEq | CmpK::Lt | CmpK::LtE | CmpK::Gt | CmpK::GtE)) {
                return FNode::Unsupported;
            }
            FNode::Cmp(b(l), ops.iter().map(|(k, r)| (*k, to_fast(r, params))).collect())
        }
        Node::Call { func, args, keywords } => {
            let Node::Name(f) = func.as_ref() else { return FNode::Unsupported };
            if *keywords != 0 {
                return FNode::Unsupported;
            }
            let fun = match f.as_str() {
                "abs" => FFun::Abs,
                "isqrt" => FFun::Isqrt,
                "gcd" => FFun::Gcd,
                "min" => FFun::Min,
                "max" => FFun::Max,
                _ => return FNode::Unsupported,
            };
            FNode::Call(fun, args.iter().map(|x| to_fast(x, params)).collect())
        }
    }
}

/// Upper limit for constants folded at compile time (bits).
const FOLD_MAX_BITS: u64 = 1 << 16;

fn fast_op(k: BinK) -> bool {
    matches!(k, BinK::Add | BinK::Sub | BinK::Mult | BinK::FloorDiv | BinK::Mod | BinK::Pow)
}

fn is_const(x: &FNode) -> bool {
    matches!(x, FNode::Int(_) | FNode::BigConst(_) | FNode::Folded(..))
}

/// Constant folding of a binary node with constant operands. The folded
/// value keeps the smallest bit limit under which its computation succeeds
/// (`fast_eval` is monotone in the limit), so folding never bypasses
/// `math_max_intermediate_bits` (F029).
fn fold(node: FNode) -> FNode {
    let FNode::Bin(fl, _, fr) = &node else { return node };
    if !(is_const(fl) && is_const(fr)) {
        return node;
    }
    let Some(v) = fast_eval(&node, &[], FOLD_MAX_BITS) else { return node };
    let (mut lo, mut hi) = (0u64, FOLD_MAX_BITS);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if fast_eval(&node, &[], mid).is_some() {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    FNode::Folded(v, lo)
}

fn floor_div(a: i64, b: i64) -> Option<i64> {
    let q = a.checked_div(b)?;
    Some(if (a % b != 0) && ((a < 0) != (b < 0)) { q - 1 } else { q })
}

fn floor_mod(a: i64, b: i64) -> Option<i64> {
    let r = a.checked_rem(b)?;
    Some(if r != 0 && ((r < 0) != (b < 0)) { r + b } else { r })
}

fn isqrt_u64(x: u64) -> u64 {
    let mut r = (x as f64).sqrt() as u64;
    while (r as u128) * (r as u128) > x as u128 {
        r -= 1;
    }
    while ((r + 1) as u128) * ((r + 1) as u128) <= x as u128 {
        r += 1;
    }
    r
}

/// One fast-path arithmetic operation (`None` ⇒ exact path).
fn fast_binop(k: BinK, a: F64v, b: F64v, max_bits: u64) -> Option<F64v> {
    let guard = |est: u64| if est > max_bits { None } else { Some(()) };
    if a.is_bool() || b.is_bool() {
        return None;
    }
    if let (F64v::Int(x), F64v::Int(y)) = (&a, &b) {
        let (x, y) = (*x, *y);
        let small = match k {
            BinK::Add => x.checked_add(y),
            BinK::Sub => x.checked_sub(y),
            BinK::Mult => x.checked_mul(y),
            BinK::FloorDiv => {
                if y == 0 {
                    return None;
                }
                floor_div(x, y)
            }
            BinK::Mod => {
                if y == 0 {
                    return None;
                }
                floor_mod(x, y)
            }
            BinK::Pow => {
                if y < 0 {
                    return None;
                }
                u32::try_from(y).ok().and_then(|e| x.checked_pow(e))
            }
            _ => return None,
        };
        if let Some(v) = small {
            return Some(F64v::Int(v));
        }
    }
    let (x, y) = (a.big(), b.big());
    let res = match k {
        BinK::Add | BinK::Sub => {
            guard(a.bits().max(b.bits()) + 1)?;
            if k == BinK::Add { x + y } else { x - y }
        }
        BinK::Mult => {
            guard(a.bits() + b.bits())?;
            x * y
        }
        BinK::FloorDiv | BinK::Mod => {
            if y.is_zero() {
                return None;
            }
            if k == BinK::FloorDiv { x.div_floor(&y) } else { x.mod_floor(&y) }
        }
        BinK::Pow => {
            let dummy = HashMap::new();
            Eval { vars: &dummy, max_bits }.pow(&x, &y).ok()?
        }
        _ => return None,
    };
    guard(res.bits())?;
    Some(F64v::norm(res))
}

/// Fast evaluation with bit limit `max_bits` (>= 128, see caller).
fn fast_eval(n: &FNode, vars: &[i64], max_bits: u64) -> Option<F64v> {
    let guard = |est: u64| if est > max_bits { None } else { Some(()) };
    Some(match n {
        FNode::Int(i) => F64v::Int(*i),
        FNode::BigConst(b) => {
            guard(b.bits())?;
            F64v::Big(b.clone())
        }
        FNode::Bool(b) => F64v::Bool(*b),
        FNode::Var(i) => F64v::Int(*vars.get(*i)?),
        FNode::Bin(l, k, r) => {
            let (a, b) = (fast_eval(l, vars, max_bits)?, fast_eval(r, vars, max_bits)?);
            fast_binop(*k, a, b, max_bits)?
        }
        FNode::Chain(first, rest) => {
            let mut acc = fast_eval(first, vars, max_bits)?;
            for (k, x) in rest {
                let b = fast_eval(x, vars, max_bits)?;
                acc = fast_binop(*k, acc, b, max_bits)?;
            }
            acc
        }
        FNode::Folded(v, need) => {
            if *need > max_bits {
                return None;
            }
            v.clone()
        }
        FNode::Pos(x) => match fast_eval(x, vars, max_bits)? {
            F64v::Bool(b) => F64v::Int(b as i64),
            other => other,
        },
        FNode::Neg(x) => match fast_eval(x, vars, max_bits)? {
            F64v::Int(i) => match i.checked_neg() {
                Some(v) => F64v::Int(v),
                None => F64v::norm(-BigInt::from(i)),
            },
            F64v::Big(b) => F64v::norm(-b),
            F64v::Bool(b) => F64v::Int(-(b as i64)),
        },
        FNode::Not(x) => F64v::Bool(!fast_eval(x, vars, max_bits)?.truthy()),
        FNode::And(v) => {
            for x in v {
                if !fast_eval(x, vars, max_bits)?.truthy() {
                    return Some(F64v::Bool(false));
                }
            }
            F64v::Bool(true)
        }
        FNode::Or(v) => {
            for x in v {
                if fast_eval(x, vars, max_bits)?.truthy() {
                    return Some(F64v::Bool(true));
                }
            }
            F64v::Bool(false)
        }
        FNode::Cmp(l, ops) => {
            let mut a = match fast_eval(l, vars, max_bits)? {
                F64v::Bool(b) => F64v::Int(b as i64),
                o => o,
            };
            for (k, r) in ops {
                let b = match fast_eval(r, vars, max_bits)? {
                    F64v::Bool(x) => F64v::Int(x as i64),
                    o => o,
                };
                let o = a.cmp(&b);
                let ok = match k {
                    CmpK::Eq => o.is_eq(),
                    CmpK::NotEq => o.is_ne(),
                    CmpK::Lt => o.is_lt(),
                    CmpK::LtE => o.is_le(),
                    CmpK::Gt => o.is_gt(),
                    CmpK::GtE => o.is_ge(),
                    _ => return None,
                };
                if !ok {
                    return Some(F64v::Bool(false));
                }
                a = b;
            }
            F64v::Bool(true)
        }
        FNode::Call(f, args) => {
            let mut xs = Vec::with_capacity(args.len());
            for a in args {
                let v = fast_eval(a, vars, max_bits)?;
                if v.is_bool() {
                    return None;
                }
                xs.push(v);
            }
            match f {
                FFun::Abs if xs.len() == 1 => match &xs[0] {
                    F64v::Int(i) => match i.checked_abs() {
                        Some(v) => F64v::Int(v),
                        None => F64v::norm(BigInt::from(*i).abs()),
                    },
                    other => F64v::norm(other.big().abs()),
                },
                FFun::Isqrt if xs.len() == 1 => match &xs[0] {
                    F64v::Int(i) if *i >= 0 => F64v::Int(isqrt_u64(*i as u64) as i64),
                    F64v::Big(b) if !b.is_negative() => F64v::norm(b.sqrt()),
                    _ => return None,
                },
                FFun::Gcd if xs.iter().all(|x| matches!(x, F64v::Int(_))) => {
                    let mut g: u64 = 0;
                    for x in &xs {
                        let F64v::Int(x) = x else { unreachable!() };
                        let mut a = x.unsigned_abs();
                        let mut b = g;
                        while b != 0 {
                            (a, b) = (b, a % b);
                        }
                        g = a;
                    }
                    F64v::norm(BigInt::from(g))
                }
                FFun::Min if !xs.is_empty() => xs.iter().min_by(|a, b| a.cmp(b)).unwrap().clone(),
                FFun::Max if !xs.is_empty() => xs.iter().max_by(|a, b| a.cmp(b)).unwrap().clone(),
                _ => return None,
            }
        }
        FNode::Unsupported => return None,
    })
}

impl PartialEq for MathExpression {
    fn eq(&self, o: &Self) -> bool {
        self.source == o.source && self.parameters == o.parameters
    }
}

pub fn check_parameters(params: &[String]) -> Result<()> {
    if params.is_empty() {
        return Err(syn("at least one parameter is required"));
    }
    let set: HashSet<&String> = params.iter().collect();
    if set.len() != params.len() {
        return Err(syn("parameters must be unique"));
    }
    for name in params {
        if !unicode::is_identifier(name) || name.starts_with('_') {
            return Err(syn(format!("invalid parameter name {}", Val::str(name.as_str()).py_repr())));
        }
        if unicode::identifier_needs_nfkc(name) {
            // F043: Python normalizes names in the source, so such a parameter could never match
            return Err(syn(format!(
                "parameter name {} is not NFKC-normalized (please use the normalized form)",
                Val::str(name.as_str()).py_repr()
            )));
        }
        if SAFE_FUNCTIONS.contains(&name.as_str()) {
            return Err(syn(format!("parameter name {} collides with a math function", Val::str(name.as_str()).py_repr())));
        }
    }
    Ok(())
}

impl MathExpression {
    /// `MathExpression.compile(source, parameters)`
    pub fn compile(source: &str, parameters: &[String]) -> Result<MathExpression> {
        let source = unicode::strip(source).to_string();
        let params: Vec<String> = parameters.iter().map(|p| unicode::strip(p).to_string()).collect();
        if source.is_empty() {
            return Err(syn("empty mathematical expression"));
        }
        check_parameters(&params)?;
        let normalized = source.replace('^', "**");
        let tree = parse(&normalized)?;
        let pset: HashSet<String> = params.iter().cloned().collect();
        validate(&tree, &pset)?;
        let fast = to_fast(&tree, &params);
        Ok(MathExpression { source, parameters: params, tree, fast })
    }

    /// i64 fast path (values in parameter order); `None` ⇒ exact path.
    fn evaluate_fast(&self, vars: &[i64], max_bits: u64) -> Option<F64v> {
        fast_eval(&self.fast, vars, max_bits)
    }

    /// `evaluate(**variables)`
    pub fn evaluate(&self, variables: &HashMap<String, BigInt>, max_bits: u64) -> Result<MV> {
        let missing: Vec<&str> = self.parameters.iter().filter(|p| !variables.contains_key(*p)).map(|s| s.as_str()).collect();
        if !missing.is_empty() {
            return Err(dom(format!("missing variables: {}", missing.join(", "))));
        }
        let mut extra: Vec<&str> = variables.keys().filter(|k| !self.parameters.contains(k)).map(|s| s.as_str()).collect();
        if !extra.is_empty() {
            extra.sort();
            return Err(dom(format!("unknown variables: {}", extra.join(", "))));
        }
        Eval { vars: variables, max_bits }.visit(&self.tree)
    }

    pub fn to_val(&self) -> Val {
        vmap! {"source" => self.source.as_str(), "parameters" => Val::List(self.parameters.iter().map(|p| Val::str(p.as_str())).collect())}
    }
}

// ----------------------------------------------------------------------
// MathMap
// ----------------------------------------------------------------------

/// `normalize_side`: NL/NR, 0/1, False/True.
pub fn normalize_side(v: &Val) -> Result<i64> {
    match v {
        Val::Str(s) if s == "NL" || s == "nl" => Ok(NL),
        Val::Str(s) if s == "NR" || s == "nr" => Ok(NR),
        Val::Bool(b) => Ok(*b as i64),
        Val::Int(0) => Ok(NL),
        Val::Int(1) => Ok(NR),
        Val::Float(f) if *f == 0.0 => Ok(NL),
        Val::Float(f) if *f == 1.0 => Ok(NR),
        _ => Err(dom("side must be NL/NR or 0/1")),
    }
}

/// `split_global_v(V, realm_span)` → (logical realm, local v).
pub fn split_global_v(v: &BigInt, span: &BigInt) -> Result<(BigInt, BigInt)> {
    if !v.is_positive() {
        return Err(dom("V must be > 0"));
    }
    if !span.is_positive() {
        return Err(dom("realm_span must be a positive integer"));
    }
    let (q, r) = (v - BigInt::one()).div_mod_floor(span);
    Ok((q + 1, r + 1))
}

/// `join_global_v(logical_realm, v_local, realm_span)`
pub fn join_global_v(realm: &BigInt, v_local: &BigInt, span: &BigInt) -> Result<BigInt> {
    if !realm.is_positive() {
        return Err(dom("logical_realm must be >= 1"));
    }
    if !v_local.is_positive() || v_local > span {
        return Err(dom("v_local is outside the realm"));
    }
    Ok((realm - BigInt::one()) * span + v_local)
}

/// Logical cell key `(logical_realm, u, v_local, side)`.
pub type LogicalCellKey = (BigInt, BigInt, BigInt, i64);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MathMapTarget {
    pub inputs: Vec<BigInt>,
    pub u: BigInt,
    pub v_global: BigInt,
    pub logical_realm: BigInt,
    pub v_local: BigInt,
    pub side: i64,
}

impl MathMapTarget {
    pub fn logical_cell_key(&self) -> LogicalCellKey {
        (self.logical_realm.clone(), self.u.clone(), self.v_local.clone(), self.side)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Side {
    Fixed(i64),
    Expr(MathExpression),
}

#[derive(Clone, Debug, PartialEq)]
pub struct MathMap {
    pub name: String,
    pub parameters: Vec<String>,
    pub u: MathExpression,
    pub v: MathExpression,
    pub side: Side,
    pub realm_span: BigInt,
}

impl MathMap {
    /// `MathMap.compile(name, parameters, u=, V=, side=, realm_span=)`
    pub fn compile(name: &str, parameters: &[String], u: &str, v: &str, side: &Val, realm_span: &BigInt) -> Result<MathMap> {
        let name = unicode::strip(name).to_string();
        let params: Vec<String> = parameters.iter().map(|p| unicode::strip(p).to_string()).collect();
        if name.is_empty() || !unicode::is_identifier(&name) || name.starts_with('_') {
            return Err(syn(format!("invalid mapping name {}", Val::str(name.as_str()).py_repr())));
        }
        let ue = MathExpression::compile(u, &params)?;
        let ve = MathExpression::compile(v, &params)?;
        let side = match side {
            Val::Str(s) if !matches!(unicode::lower(unicode::strip(s)).as_str(), "nl" | "nr") => {
                Side::Expr(MathExpression::compile(s, &params)?)
            }
            other => Side::Fixed(normalize_side(other)?),
        };
        Ok(MathMap { name, parameters: params, u: ue, v: ve, side, realm_span: realm_span.clone() })
    }

    /// `_bind` + `target(*inputs)`
    pub fn target(&self, inputs: &[Val], max_bits: u64) -> Result<MathMapTarget> {
        if inputs.len() != self.parameters.len() {
            return Err(dom(format!(
                "{} expects {} argument(s), got: {}",
                self.name,
                self.parameters.len(),
                inputs.len()
            )));
        }
        // fast path: all inputs i64 and bit limit above any i64 estimate
        if max_bits >= 128 {
            let small: Option<Vec<i64>> = inputs.iter().map(|v| if let Val::Int(i) = v { Some(*i) } else { None }).collect();
            if let Some(xs) = small {
                if let Some(t) = self.target_fast(&xs, max_bits) {
                    return Ok(t);
                }
            }
        }
        let mut vars = HashMap::new();
        let mut ins = Vec::with_capacity(inputs.len());
        for (name, val) in self.parameters.iter().zip(inputs) {
            let i = match val {
                Val::Int(i) => BigInt::from(*i),
                Val::Big(b) => b.clone(),
                _ => return Err(dom(format!("input {} must be an integer", Val::str(name.as_str()).py_repr()))),
            };
            ins.push(i.clone());
            vars.insert(name.clone(), i);
        }
        self.target_ints(ins, &vars, max_bits)
    }

    /// i64 fast path of `target`; `None` if a value does not fit into i64
    /// or an error occurs (then evaluated exactly via BigInt).
    fn target_fast(&self, xs: &[i64], max_bits: u64) -> Option<MathMapTarget> {
        let pos = |v: F64v| -> Option<BigInt> {
            match v {
                F64v::Int(i) if i > 0 => Some(BigInt::from(i)),
                F64v::Big(b) if b.is_positive() => Some(b),
                _ => None,
            }
        };
        let u = pos(self.u.evaluate_fast(xs, max_bits)?)?;
        let v = pos(self.v.evaluate_fast(xs, max_bits)?)?;
        let side = match &self.side {
            Side::Fixed(s) => *s,
            Side::Expr(e) => match e.evaluate_fast(xs, max_bits)? {
                F64v::Bool(b) => b as i64,
                F64v::Int(0) => NL,
                F64v::Int(1) => NR,
                _ => return None,
            },
        };
        let (realm, local) = split_global_v(&v, &self.realm_span).ok()?;
        Some(MathMapTarget { inputs: xs.iter().map(|x| BigInt::from(*x)).collect(), u, v_global: v, logical_realm: realm, v_local: local, side })
    }

    pub fn target_ints(&self, ins: Vec<BigInt>, vars: &HashMap<String, BigInt>, max_bits: u64) -> Result<MathMapTarget> {
        let u = self.u.evaluate(vars, max_bits)?;
        let v = self.v.evaluate(vars, max_bits)?;
        let u = match u {
            MV::Int(i) if i.is_positive() => i,
            _ => return Err(dom("u must be a positive integer")),
        };
        let v = match v {
            MV::Int(i) if i.is_positive() => i,
            _ => return Err(dom("V must be a positive integer")),
        };
        let side = match &self.side {
            Side::Fixed(s) => *s,
            Side::Expr(e) => match e.evaluate(vars, max_bits)? {
                MV::Bool(b) => b as i64,
                MV::Int(i) if i.is_zero() => NL,
                MV::Int(i) if i.is_one() => NR,
                _ => return Err(dom("side must be NL/NR or 0/1")),
            },
        };
        let (realm, local) = split_global_v(&v, &self.realm_span)?;
        Ok(MathMapTarget { inputs: ins, u, v_global: v, logical_realm: realm, v_local: local, side })
    }

    pub fn canonical_dict(&self) -> Val {
        let (side, is_expr) = match &self.side {
            Side::Fixed(s) => (Val::Int(*s), false),
            Side::Expr(e) => (Val::str(e.source.as_str()), true),
        };
        vmap! {
            "contract" => MAP_CONTRACT, "name" => self.name.as_str(),
            "parameters" => Val::List(self.parameters.iter().map(|p| Val::str(p.as_str())).collect()),
            "u" => self.u.source.as_str(), "V" => self.v.source.as_str(),
            "side" => side, "side_is_expression" => is_expr,
            "realm_span" => Val::from_bigint(self.realm_span.clone()),
        }
    }

    /// SHA-256 over `json.dumps(canonical_dict, sort_keys=True,
    /// separators=(",",":"))` (Python default `ensure_ascii=True`).
    pub fn fingerprint(&self) -> String {
        let j = self.canonical_dict().to_json_ascii().expect("canonical JSON");
        crate::crypto::sha256_hex(j.as_bytes())
    }

    pub fn to_val(&self) -> Val {
        let mut d = self.canonical_dict();
        d.set("fingerprint", Val::Str(self.fingerprint()));
        d
    }

    pub fn from_val(raw: &Val) -> Result<MathMap> {
        if raw.get("contract").map(|v| v.py_str()) != Some(MAP_CONTRACT.into()) {
            return Err(dom("unknown MATH_MAP contract"));
        }
        let get = |k: &str| raw.get(k).cloned().ok_or_else(|| Error::Key(Val::str(k).py_repr()));
        let side = if raw.get("side_is_expression").map(|v| v.is_truthy()).unwrap_or(false) {
            Val::Str(get("side")?.py_str())
        } else {
            let s = get("side")?;
            Val::Int(s.as_i64().ok_or_else(|| Error::Value(format!("invalid literal for int(): {}", s.py_repr())))?)
        };
        let params: Vec<String> = match get("parameters")? {
            Val::List(l) => l.iter().map(|x| x.py_str()).collect(),
            Val::Str(s) => s.chars().map(|c| c.to_string()).collect(),
            other => return Err(Error::Type(format!("invalid parameters: {}", other.py_repr()))),
        };
        let span = match raw.get("realm_span") {
            None => crate::address::realm_span().to_bigint(),
            Some(v) => v.as_bigint().ok_or_else(|| Error::Value("invalid realm_span".into()))?,
        };
        let m = MathMap::compile(&get("name")?.py_str(), &params, &get("u")?.py_str(), &get("V")?.py_str(), &side, &span)?;
        if let Some(fp) = raw.get("fingerprint").filter(|v| !v.is_none()) {
            if fp.py_str() != m.fingerprint() {
                return Err(dom("MATH_MAP fingerprint does not match"));
            }
        }
        Ok(m)
    }
}

// ----------------------------------------------------------------------
// MathMapLayoutSpec
// ----------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct MathMapLayoutSpec {
    pub layout_name: String,
    pub mapping: MathMap,
    pub realm_auto_limit: BigInt,
    pub description: String,
}

impl MathMapLayoutSpec {
    pub fn new(layout_name: &str, mapping: MathMap, realm_auto_limit: BigInt, description: &str) -> Result<Self> {
        if layout_name.is_empty() {
            return Err(dom("layout_name must not be empty"));
        }
        if !realm_auto_limit.is_positive() {
            return Err(dom("realm_auto_limit must be > 0"));
        }
        Ok(MathMapLayoutSpec { layout_name: layout_name.into(), mapping, realm_auto_limit, description: description.into() })
    }

    pub fn fingerprint(&self) -> String {
        self.mapping.fingerprint()
    }

    pub fn to_val(&self) -> Val {
        vmap! {
            "kind" => "MATH", "math_contract" => MAP_CONTRACT, "layout_name" => self.layout_name.as_str(),
            "mapping" => self.mapping.to_val(), "realm_auto_limit" => Val::from_bigint(self.realm_auto_limit.clone()),
            "description" => self.description.as_str(),
        }
    }

    pub fn from_val(raw: &Val) -> Result<Self> {
        if unicode::upper(&raw.get("kind").map(|v| v.py_str()).unwrap_or_default()) != "MATH" {
            return Err(dom("not a MATH LayoutSpec"));
        }
        let mapping = MathMap::from_val(raw.get("mapping").ok_or_else(|| Error::Key("'mapping'".into()))?)?;
        let limit = match raw.get("realm_auto_limit") {
            None => BigInt::from(crate::config::DEFAULT_REALM_AUTO_LIMIT),
            Some(v) => v.as_bigint().ok_or_else(|| Error::Value("invalid realm_auto_limit".into()))?,
        };
        let name = raw.get("layout_name").ok_or_else(|| Error::Key("'layout_name'".into()))?.py_str();
        let desc = raw.get("description").map(|v| v.py_str()).unwrap_or_default();
        MathMapLayoutSpec::new(&name, mapping, limit, &desc)
    }
}

// ----------------------------------------------------------------------
// dry_run
// ----------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct DryRunReport {
    pub total: u64,
    pub valid: u64,
    pub invalid: u64,
    pub unique_cells: u64,
    pub collisions: u64,
    pub collision_examples: Vec<(Vec<Val>, LogicalCellKey)>,
    pub logical_realms: Vec<BigInt>,
    pub realm_budget_exceeded: bool,
    pub min_u: Option<BigInt>,
    pub max_u: Option<BigInt>,
    pub min_v: Option<BigInt>,
    pub max_v: Option<BigInt>,
    pub nl_count: u64,
    pub nr_count: u64,
    pub invalid_examples: Vec<(Vec<Val>, String)>,
}

/// Collects `dry_run` incrementally (rows come from Python iterators).
pub struct DryRun {
    pub report: DryRunReport,
    seen: Option<HashSet<(BigInt, BigInt, BigInt, i64)>>,
    realms: std::collections::BTreeSet<BigInt>,
    max_examples: usize,
    realm_auto_limit: BigInt,
}

impl DryRun {
    pub fn new(realm_auto_limit: BigInt, track_collisions: bool, max_examples: usize) -> Self {
        DryRun {
            report: DryRunReport::default(),
            seen: if track_collisions { Some(HashSet::new()) } else { None },
            realms: Default::default(),
            max_examples,
            realm_auto_limit,
        }
    }

    pub fn add(&mut self, mapping: &MathMap, row: &[Val], max_bits: u64) {
        let r = &mut self.report;
        r.total += 1;
        let t = match mapping.target(row, max_bits) {
            Ok(t) => t,
            Err(Error::MathSyntax(m)) | Err(Error::MathDomain(m)) => {
                r.invalid += 1;
                if r.invalid_examples.len() < self.max_examples {
                    r.invalid_examples.push((row.to_vec(), m));
                }
                return;
            }
            Err(e) => {
                r.invalid += 1;
                if r.invalid_examples.len() < self.max_examples {
                    r.invalid_examples.push((row.to_vec(), e.to_string()));
                }
                return;
            }
        };
        r.valid += 1;
        self.realms.insert(t.logical_realm.clone());
        let upd = |cur: &mut Option<BigInt>, x: &BigInt, min: bool| {
            if cur.as_ref().is_none_or(|c| if min { x < c } else { x > c }) {
                *cur = Some(x.clone());
            }
        };
        upd(&mut r.min_u, &t.u, true);
        upd(&mut r.max_u, &t.u, false);
        upd(&mut r.min_v, &t.v_global, true);
        upd(&mut r.max_v, &t.v_global, false);
        if t.side == NL {
            r.nl_count += 1;
        } else {
            r.nr_count += 1;
        }
        if let Some(seen) = &mut self.seen {
            let key = t.logical_cell_key();
            if seen.contains(&key) {
                r.collisions += 1;
                if r.collision_examples.len() < self.max_examples {
                    r.collision_examples.push((row.to_vec(), key));
                }
            } else {
                seen.insert(key);
            }
        }
    }

    pub fn finish(mut self) -> DryRunReport {
        self.report.logical_realms = self.realms.into_iter().collect();
        self.report.unique_cells = match &self.seen {
            Some(s) => s.len() as u64,
            None => self.report.valid,
        };
        self.report.realm_budget_exceeded = BigInt::from(self.report.logical_realms.len()) > self.realm_auto_limit;
        self.report
    }
}

/// `_coalesce_positive_ints`
pub fn coalesce(values: &[BigInt]) -> Vec<(BigInt, BigInt)> {
    let mut xs: Vec<BigInt> = values.to_vec();
    xs.sort();
    xs.dedup();
    let mut out: Vec<(BigInt, BigInt)> = Vec::new();
    for x in xs {
        match out.last_mut() {
            Some((_, b)) if b.clone() + 1 == x => *b = x,
            _ => out.push((x.clone(), x)),
        }
    }
    out
}

pub fn sign_of(b: &BigInt) -> Sign {
    b.sign()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    fn eval(src: &str, x: i64) -> Result<MV> {
        let e = MathExpression::compile(src, &p(&["x"]))?;
        let mut v = HashMap::new();
        v.insert("x".into(), BigInt::from(x));
        e.evaluate(&v, default_max_bits())
    }

    #[test]
    fn python_semantics() {
        assert_eq!(eval("-2**2", 0).unwrap(), MV::Int(BigInt::from(-4)));
        assert_eq!(eval("2**3**2", 0).unwrap(), MV::Int(BigInt::from(512)));
        assert_eq!(eval("x^2", 7).unwrap(), MV::Int(BigInt::from(49)));
        assert_eq!(eval("-7 // 2", 0).unwrap(), MV::Int(BigInt::from(-4)));
        assert_eq!(eval("-7 % 3", 0).unwrap(), MV::Int(BigInt::from(2)));
        assert_eq!(eval("7 % -3", 0).unwrap(), MV::Int(BigInt::from(-2)));
        assert_eq!(eval("1 < x < 5", 3).unwrap(), MV::Bool(true));
        assert_eq!(eval("1 < x < 5", 7).unwrap(), MV::Bool(false));
        assert_eq!(eval("-(x > 1)", 3).unwrap(), MV::Int(BigInt::from(-1)));
        assert_eq!(eval("0x1F + 0o7 + 0b11 + 1_000", 0).unwrap(), MV::Int(BigInt::from(31 + 7 + 3 + 1000)));
        assert_eq!(eval("gcd() + lcm()", 0).unwrap(), MV::Int(BigInt::from(1)));
        assert_eq!(eval("isqrt(10**40)", 0).unwrap(), MV::Int(BigInt::from(10u64).pow(20)));
        assert!(matches!(eval("x / 2", 1), Err(Error::MathSyntax(m)) if m == "syntax element Div is not allowed"));
        assert!(matches!(eval("x.real", 1), Err(Error::MathSyntax(m)) if m == "syntax element Attribute is not allowed"));
        assert!(matches!(eval("__import__('os')", 1), Err(Error::MathSyntax(m)) if m == "only approved mathematical functions are allowed"));
        assert!(matches!(eval("y + 1", 1), Err(Error::MathSyntax(m)) if m == "unknown name 'y'"));
        assert!(matches!(eval("1.5", 1), Err(Error::MathSyntax(m)) if m == "only integer constants are allowed"));
        assert!(matches!(eval("2**-1", 1), Err(Error::MathDomain(m)) if m == "arithmetic expression must yield an integer"));
        assert!(matches!(eval("x + True", 1), Err(Error::MathDomain(_))));
        assert!(matches!(eval("abs + 1", 1), Err(Error::MathDomain(m)) if m == "variable 'abs' has no value"));
        assert!(matches!(eval("min()", 1), Err(Error::MathDomain(_))));
    }

    #[test]
    fn resource_limit_before_computation() {
        let t = std::time::Instant::now();
        assert!(matches!(eval("2**10**10", 1), Err(Error::MathDomain(m)) if m.contains("math_max_intermediate_bits")));
        assert!(matches!(eval("x**x", 10_000_000), Err(Error::MathDomain(_))));
        assert!(t.elapsed().as_millis() < 500);
        assert!(eval("2**1000000", 1).is_ok());
    }

    fn compile_x(src: &str) -> Result<MathExpression> {
        MathExpression::compile(src, &p(&["x"]))
    }

    #[test]
    fn nesting_limits_f017() {
        // run on a thread with a modest stack: the limits must keep recursion bounded
        std::thread::Builder::new()
            .stack_size(4 << 20)
            .spawn(|| {
                let par = |n: usize| format!("{}x{}", "(".repeat(n), ")".repeat(n));
                assert_eq!(eval(&par(200), 3).unwrap(), MV::Int(BigInt::from(3)));
                assert!(matches!(compile_x(&par(201)), Err(Error::MathSyntax(m)) if m.contains("too many nested parentheses")));
                assert!(matches!(compile_x(&par(100_000)), Err(Error::MathSyntax(_))));
                let neg = |n: usize| format!("{}x", "-".repeat(n));
                assert_eq!(eval(&neg(200), 3).unwrap(), MV::Int(BigInt::from(3)));
                assert!(matches!(compile_x(&neg(201)), Err(Error::MathSyntax(m)) if m.contains("too deeply nested")));
                assert!(matches!(compile_x(&neg(100_000)), Err(Error::MathSyntax(_))));
                assert!(matches!(compile_x(&format!("{}x", "not ".repeat(100_000))), Err(Error::MathSyntax(_))));
                assert!(matches!(compile_x(&vec!["x"; 100_000].join("**")), Err(Error::MathSyntax(_))));
                assert!(matches!(compile_x(&format!("{}x{}", "abs(".repeat(5000), ")".repeat(5000))), Err(Error::MathSyntax(_))));
                // long chains are n-ary: evaluated iteratively, fast and exact path agree
                let sum = format!("x{}", "+x".repeat(99_999));
                assert_eq!(eval(&sum, 3).unwrap(), MV::Int(BigInt::from(300_000)));
                let e = compile_x(&sum).unwrap();
                assert_eq!(e.evaluate_fast(&[3], 1 << 20), Some(F64v::Int(300_000)));
                let mixed = format!("x{}", "-1+2".repeat(50_000));
                assert_eq!(eval(&mixed, 0).unwrap(), MV::Int(BigInt::from(50_000)));
                assert_eq!(eval("10 - 3 - 2 + 1 * 4 // 3 % 5", 0).unwrap(), MV::Int(BigInt::from(6)));
                assert_eq!(eval("x - 1 - 1 - 1", 0).unwrap(), MV::Int(BigInt::from(-3)));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn constant_folding_respects_bit_limit_f029() {
        let m = MathMap::compile("t", &p(&["x"]), "2**200//2**190 + x", "1", &Val::str("NL"), &crate::address::realm_span().to_bigint()).unwrap();
        for x in [Val::Int(5), Val::Big(BigInt::from(1u8) << 70)] {
            assert!(matches!(m.target(std::slice::from_ref(&x), 128), Err(Error::MathDomain(_))), "{x:?}");
            assert!(m.target(&[x], 201).is_ok());
        }
        // differential fast vs exact over several limits
        let srcs = ["10**100 * 3 + x", "2**127 + x - 2**127", "x * 2**64 // 2**64", "(2**130 - 2**130) + x", "3**80 % 7 + x"];
        for src in srcs {
            let e = compile_x(src).unwrap();
            for bits in [128u64, 129, 130, 131, 200, 256, 400, 1 << 20] {
                for x in [1i64, 5, -7, i64::MAX] {
                    let mut v = HashMap::new();
                    v.insert("x".to_string(), BigInt::from(x));
                    match (e.evaluate(&v, bits), e.evaluate_fast(&[x], bits)) {
                        (Ok(s), Some(f)) => assert_eq!(MV::Int(f.big()), s, "{src} {bits} {x}"),
                        (Err(_), Some(f)) => panic!("fast path bypasses the limit: {src} {bits} {x} -> {f:?}"),
                        _ => {}
                    }
                }
            }
        }
    }

    #[test]
    fn mapping_fingerprint_matches_python() {
        // python: MathMap.compile("demo", ("x",), u="x+1", V="x+1", side="NL").fingerprint
        let m = MathMap::compile("demo", &p(&["x"]), "x+1", "x+1", &Val::str("NL"), &crate::address::realm_span().to_bigint()).unwrap();
        assert!(m.canonical_dict().to_json_ascii().unwrap().len() > 10);
        let t = m.target(&[Val::Int(1)], default_max_bits()).unwrap();
        assert_eq!((t.logical_realm.clone(), t.v_local.clone(), t.side), (BigInt::from(1), BigInt::from(2), 0));
    }
}
