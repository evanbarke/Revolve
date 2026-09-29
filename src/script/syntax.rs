//! Revolve Script syntax: tokens, AST and a recursive-descent parser for the
//! small Python-shaped language AI models write programs in.

use super::Diag;

// ------------------------------------------------------------------ AST

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub body: Vec<Stmt>,
    pub funcs: Vec<FuncDef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuncDef {
    pub name: String,
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    Assign(String, Expr),
    AugAssign(String, BinOp, Expr),
    If(Vec<(Expr, Vec<Stmt>)>, Vec<Stmt>),
    For(String, Expr, Vec<Stmt>),
    Break,
    Continue,
    Pass,
    Return(Option<Expr>),
    Expect(Expr, String),
    Expr(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    FloorDiv,
    Mod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    NotIn,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Int(i64),
    Str(String),
    Bool(bool),
    None,
    Name(String),
    List(Vec<Expr>),
    /// `[elt for var in iter if cond]`
    ListComp {
        elt: Box<Expr>,
        var: String,
        iter: Box<Expr>,
        cond: Option<Box<Expr>>,
    },
    Attr(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    /// `f(args, key=value)`; `func` is a Name (function) or Attr (method).
    Call {
        func: Box<Expr>,
        args: Vec<Expr>,
        kwargs: Vec<(String, Expr)>,
    },
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    /// `a if cond else b`
    IfExp(Box<Expr>, Box<Expr>, Box<Expr>),
}

// ------------------------------------------------------------------ lexer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Name(String),
    Int(i64),
    Str(String),
    Op(&'static str),
    Newline,
    Indent,
    Dedent,
    Eof,
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    line: u32,
    col: u32,
}

const OPS: [&str; 24] = [
    "//=", "==", "!=", "<=", ">=", "+=", "-=", "*=", "//", "->", "**", "(", ")", "[", "]", "{",
    "}", ",", ":", ".", "+", "-", "*", "%",
];
const OPS1: [&str; 4] = ["<", ">", "=", "/"];

fn diag(line: u32, col: u32, msg: impl Into<String>) -> Diag {
    Diag {
        line,
        col,
        message: msg.into(),
    }
}

fn lex(src: &str) -> Result<Vec<Token>, Diag> {
    let mut out: Vec<Token> = vec![];
    let mut indents: Vec<usize> = vec![0];
    let mut depth = 0usize; // bracket nesting: newlines and indentation are ignored inside
    for (li, raw) in src.lines().enumerate() {
        let line = li as u32 + 1;
        let chars: Vec<char> = raw.chars().collect();
        let mut i = 0usize;
        if depth == 0 {
            // Indentation (spaces; a tab counts as 4 spaces).
            let mut width = 0usize;
            while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                width += if chars[i] == '\t' { 4 } else { 1 };
                i += 1;
            }
            if i >= chars.len() || chars[i] == '#' {
                continue; // blank or comment-only line
            }
            let cur = *indents.last().unwrap();
            if width > cur {
                indents.push(width);
                out.push(Token {
                    tok: Tok::Indent,
                    line,
                    col: 1,
                });
            } else {
                while width < *indents.last().unwrap() {
                    indents.pop();
                    out.push(Token {
                        tok: Tok::Dedent,
                        line,
                        col: 1,
                    });
                }
                if width != *indents.last().unwrap() {
                    return Err(diag(
                        line,
                        1,
                        "indentation does not match any enclosing block",
                    ));
                }
            }
        }
        while i < chars.len() {
            let c = chars[i];
            let col = i as u32 + 1;
            if c == ' ' || c == '\t' || c == '\r' {
                i += 1;
                continue;
            }
            if c == '#' {
                break;
            }
            if c.is_ascii_digit() {
                let st = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
                    i += 1;
                }
                if i < chars.len()
                    && chars[i] == '.'
                    && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())
                {
                    return Err(diag(
                        line,
                        col,
                        "decimal numbers are not supported; use whole numbers",
                    ));
                }
                let s: String = chars[st..i].iter().filter(|c| **c != '_').collect();
                let n: i64 = s
                    .parse()
                    .map_err(|_| diag(line, col, "number is too large"))?;
                if n > i32::MAX as i64 {
                    return Err(diag(line, col, "number is too large"));
                }
                out.push(Token {
                    tok: Tok::Int(n),
                    line,
                    col,
                });
                continue;
            }
            if c.is_alphabetic() || c == '_' {
                let st = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[st..i].iter().collect();
                if (word == "f" || word == "F" || word == "rf" || word == "fr")
                    && i < chars.len()
                    && (chars[i] == '"' || chars[i] == '\'')
                {
                    return Err(diag(
                        line,
                        col,
                        "f-strings are not supported; join text with + and str(value)",
                    ));
                }
                out.push(Token {
                    tok: Tok::Name(word),
                    line,
                    col,
                });
                continue;
            }
            if c == '"' || c == '\'' {
                let q = c;
                if chars.get(i + 1) == Some(&q) && chars.get(i + 2) == Some(&q) {
                    return Err(diag(
                        line,
                        col,
                        "triple-quoted strings are not supported; use # comments",
                    ));
                }
                i += 1;
                let mut s = String::new();
                loop {
                    let Some(&d) = chars.get(i) else {
                        return Err(diag(line, col, "string is not closed on this line"));
                    };
                    i += 1;
                    if d == q {
                        break;
                    }
                    if d == '\\' {
                        let Some(&e) = chars.get(i) else {
                            return Err(diag(line, col, "string is not closed on this line"));
                        };
                        i += 1;
                        s.push(match e {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        });
                    } else {
                        s.push(d);
                    }
                }
                out.push(Token {
                    tok: Tok::Str(s),
                    line,
                    col,
                });
                continue;
            }
            let rest: String = chars[i..].iter().take(3).collect();
            if let Some(op) = OPS.iter().find(|o| rest.starts_with(**o)) {
                match *op {
                    "(" | "[" | "{" => depth += 1,
                    ")" | "]" | "}" => depth = depth.saturating_sub(1),
                    _ => {}
                }
                i += op.chars().count();
                out.push(Token {
                    tok: Tok::Op(op),
                    line,
                    col,
                });
                continue;
            }
            if let Some(op) = OPS1.iter().find(|o| rest.starts_with(**o)) {
                if *op == "/" {
                    return Err(diag(line, col, "use // for division (whole numbers only)"));
                }
                i += 1;
                out.push(Token {
                    tok: Tok::Op(op),
                    line,
                    col,
                });
                continue;
            }
            return Err(diag(line, col, format!("unexpected character `{c}`")));
        }
        if depth == 0
            && out.last().is_some_and(|t| {
                t.tok != Tok::Newline && t.tok != Tok::Indent && t.tok != Tok::Dedent
            })
        {
            out.push(Token {
                tok: Tok::Newline,
                line,
                col: chars.len() as u32 + 1,
            });
        }
    }
    let last = src.lines().count() as u32 + 1;
    if depth > 0 {
        return Err(diag(last - 1, 1, "a bracket is not closed"));
    }
    if out.last().is_some_and(|t| t.tok != Tok::Newline) {
        out.push(Token {
            tok: Tok::Newline,
            line: last,
            col: 1,
        });
    }
    while indents.len() > 1 {
        indents.pop();
        out.push(Token {
            tok: Tok::Dedent,
            line: last,
            col: 1,
        });
    }
    out.push(Token {
        tok: Tok::Eof,
        line: last,
        col: 1,
    });
    Ok(out)
}

// ------------------------------------------------------------------ parser

const UNSUPPORTED: [(&str, &str); 12] = [
    (
        "while",
        "while loops are not allowed (programs must always finish); loop over a list with `for`",
    ),
    (
        "import",
        "imports are not available; everything the program can use is built in",
    ),
    (
        "from",
        "imports are not available; everything the program can use is built in",
    ),
    (
        "class",
        "classes are not supported; use functions and lists",
    ),
    (
        "lambda",
        "lambda is not supported; define a function with def",
    ),
    (
        "try",
        "try/except is not supported; actions return True or False instead of raising",
    ),
    ("with", "with is not supported"),
    (
        "global",
        "global is not supported; pass values as parameters and return results",
    ),
    ("nonlocal", "nonlocal is not supported"),
    ("yield", "generators are not supported"),
    ("async", "async code is not supported"),
    ("del", "del is not supported"),
];

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.toks[self.pos]
    }
    fn next(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn is_op(&self, op: &str) -> bool {
        matches!(&self.peek().tok, Tok::Op(o) if *o == op)
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(&self.peek().tok, Tok::Name(n) if n == kw)
    }
    fn err_here(&self, msg: impl Into<String>) -> Diag {
        let t = self.peek();
        diag(t.line, t.col, msg)
    }
    fn expect_op(&mut self, op: &str, what: &str) -> Result<Token, Diag> {
        if self.is_op(op) {
            Ok(self.next())
        } else {
            Err(self.err_here(format!("expected `{op}` {what}")))
        }
    }
    fn expect_name(&mut self, what: &str) -> Result<(String, u32, u32), Diag> {
        let t = self.next();
        match t.tok {
            Tok::Name(n) if !is_keyword(&n) => Ok((n, t.line, t.col)),
            _ => Err(diag(t.line, t.col, format!("expected {what}"))),
        }
    }
    fn expect_newline(&mut self) -> Result<(), Diag> {
        match self.peek().tok {
            Tok::Newline => {
                self.next();
                Ok(())
            }
            Tok::Eof => Ok(()),
            _ => Err(self.err_here("unexpected text at the end of the line")),
        }
    }

    fn program(&mut self) -> Result<Program, Diag> {
        let mut body = vec![];
        let mut funcs = vec![];
        loop {
            match self.peek().tok {
                Tok::Eof => break,
                Tok::Newline => {
                    self.next();
                }
                Tok::Indent => return Err(self.err_here("unexpected indentation")),
                _ if self.is_kw("def") => funcs.push(self.funcdef()?),
                _ => body.push(self.stmt(false)?),
            }
        }
        Ok(Program { body, funcs })
    }

    fn funcdef(&mut self) -> Result<FuncDef, Diag> {
        let line = self.next().line; // def
        let (name, _, _) = self.expect_name("a function name after def")?;
        self.expect_op("(", "after the function name")?;
        let mut params = vec![];
        if !self.is_op(")") {
            loop {
                let (p, l, c) = self.expect_name("a parameter name")?;
                if self.is_op("=") {
                    return Err(diag(l, c, "default parameter values are not supported"));
                }
                if self.is_op(":") {
                    return Err(diag(l, c, "type annotations are not supported"));
                }
                params.push(p);
                if self.is_op(",") {
                    self.next();
                    if self.is_op(")") {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.expect_op(")", "after the parameters")?;
        if self.is_op("->") {
            return Err(self.err_here("return type annotations are not supported"));
        }
        let body = self.block(true)?;
        Ok(FuncDef {
            name,
            params,
            body,
            line,
        })
    }

    fn block(&mut self, in_func: bool) -> Result<Vec<Stmt>, Diag> {
        self.expect_op(":", "to start the block")?;
        if !matches!(self.peek().tok, Tok::Newline) {
            // `if x: stmt` on one line
            return Ok(vec![self.simple_stmt(in_func)?]);
        }
        self.next();
        if !matches!(self.peek().tok, Tok::Indent) {
            return Err(self.err_here("expected an indented block"));
        }
        self.next();
        let mut out = vec![];
        loop {
            match self.peek().tok {
                Tok::Dedent => {
                    self.next();
                    break;
                }
                Tok::Eof => break,
                Tok::Newline => {
                    self.next();
                }
                _ if self.is_kw("def") => {
                    return Err(self.err_here("functions must be defined at the top level"));
                }
                _ => out.push(self.stmt(in_func)?),
            }
        }
        Ok(out)
    }

    fn stmt(&mut self, in_func: bool) -> Result<Stmt, Diag> {
        let t = self.peek().clone();
        if let Tok::Name(n) = &t.tok {
            if let Some((_, why)) = UNSUPPORTED.iter().find(|(k, _)| k == n) {
                return Err(diag(t.line, t.col, *why));
            }
            match n.as_str() {
                "if" => {
                    self.next();
                    let mut branches = vec![];
                    let c = self.expr()?;
                    let b = self.block(in_func)?;
                    branches.push((c, b));
                    let mut orelse = vec![];
                    loop {
                        if self.is_kw("elif") {
                            self.next();
                            let c = self.expr()?;
                            let b = self.block(in_func)?;
                            branches.push((c, b));
                        } else if self.is_kw("else") {
                            self.next();
                            orelse = self.block(in_func)?;
                            break;
                        } else {
                            break;
                        }
                    }
                    return Ok(Stmt {
                        kind: StmtKind::If(branches, orelse),
                        line: t.line,
                    });
                }
                "for" => {
                    self.next();
                    let (var, l, c) = self.expect_name("a loop variable after for")?;
                    if self.is_op(",") {
                        return Err(diag(
                            l,
                            c,
                            "unpacking several loop variables is not supported; loop over one value",
                        ));
                    }
                    if !self.is_kw("in") {
                        return Err(self.err_here("expected `in` after the loop variable"));
                    }
                    self.next();
                    let iter = self.expr()?;
                    let body = self.block(in_func)?;
                    return Ok(Stmt {
                        kind: StmtKind::For(var, iter, body),
                        line: t.line,
                    });
                }
                "else" | "elif" => {
                    return Err(diag(t.line, t.col, format!("`{n}` without a matching if")));
                }
                _ => {}
            }
        }
        let s = self.simple_stmt(in_func)?;
        Ok(s)
    }

    fn simple_stmt(&mut self, in_func: bool) -> Result<Stmt, Diag> {
        let t = self.peek().clone();
        let line = t.line;
        let kind = match &t.tok {
            Tok::Name(n) if n == "break" => {
                self.next();
                StmtKind::Break
            }
            Tok::Name(n) if n == "continue" => {
                self.next();
                StmtKind::Continue
            }
            Tok::Name(n) if n == "pass" => {
                self.next();
                StmtKind::Pass
            }
            Tok::Name(n) if n == "return" => {
                if !in_func {
                    return Err(diag(
                        t.line,
                        t.col,
                        "return is only allowed inside a function",
                    ));
                }
                self.next();
                if matches!(self.peek().tok, Tok::Newline | Tok::Eof) {
                    StmtKind::Return(None)
                } else {
                    StmtKind::Return(Some(self.expr()?))
                }
            }
            Tok::Name(n) if n == "expect" => {
                self.next();
                let cond = self.expr()?;
                if !self.is_op(",") {
                    return Err(self.err_here(
                        "an expectation needs a description: expect <condition>, \"what it checks\"",
                    ));
                }
                self.next();
                let d = self.next();
                let Tok::Str(desc) = d.tok else {
                    return Err(diag(
                        d.line,
                        d.col,
                        "the expectation's description must be a string",
                    ));
                };
                StmtKind::Expect(cond, desc)
            }
            Tok::Name(n) if n == "assert" => {
                return Err(diag(
                    t.line,
                    t.col,
                    "use `expect <condition>, \"description\"` instead of assert",
                ));
            }
            _ => {
                let e = self.expr()?;
                if self.is_op("=") {
                    let op = self.next();
                    let ExprKind::Name(target) = e.kind else {
                        return Err(diag(
                            op.line,
                            op.col,
                            match e.kind {
                                ExprKind::Index(..) => {
                                    "assigning to an item (x[i] = ...) is not supported; build a new list"
                                }
                                ExprKind::Attr(..) => "assigning to an attribute is not supported",
                                _ => "you can only assign to a variable name",
                            },
                        ));
                    };
                    if is_keyword(&target) {
                        return Err(diag(
                            e.line,
                            e.col,
                            format!("`{target}` is a reserved word"),
                        ));
                    }
                    let v = self.expr()?;
                    if self.is_op("=") {
                        return Err(self.err_here("chained assignment is not supported"));
                    }
                    StmtKind::Assign(target, v)
                } else if self.is_op("+=")
                    || self.is_op("-=")
                    || self.is_op("*=")
                    || self.is_op("//=")
                {
                    let op = self.next();
                    let ExprKind::Name(target) = e.kind else {
                        return Err(diag(op.line, op.col, "you can only update a variable name"));
                    };
                    let bop = match op.tok {
                        Tok::Op("+=") => BinOp::Add,
                        Tok::Op("-=") => BinOp::Sub,
                        Tok::Op("*=") => BinOp::Mul,
                        _ => BinOp::FloorDiv,
                    };
                    StmtKind::AugAssign(target, bop, self.expr()?)
                } else if self.is_op(",") {
                    return Err(self.err_here("tuples and multiple assignment are not supported"));
                } else {
                    StmtKind::Expr(e)
                }
            }
        };
        self.expect_newline()?;
        Ok(Stmt { kind, line })
    }

    // expr := or_expr ['if' or_expr 'else' expr]
    fn expr(&mut self) -> Result<Expr, Diag> {
        let e = self.or_expr()?;
        if self.is_kw("if") {
            // only as `a if c else b` inside an expression
            let save = self.pos;
            self.next();
            let c = self.or_expr()?;
            if !self.is_kw("else") {
                self.pos = save;
                return Err(self.err_here("expected `else` in the conditional expression"));
            }
            self.next();
            let b = self.expr()?;
            return Ok(Expr {
                line: e.line,
                col: e.col,
                kind: ExprKind::IfExp(Box::new(e), Box::new(c), Box::new(b)),
            });
        }
        Ok(e)
    }

    fn or_expr(&mut self) -> Result<Expr, Diag> {
        let mut e = self.and_expr()?;
        while self.is_kw("or") {
            self.next();
            let r = self.and_expr()?;
            e = Expr {
                line: e.line,
                col: e.col,
                kind: ExprKind::Or(Box::new(e), Box::new(r)),
            };
        }
        Ok(e)
    }

    fn and_expr(&mut self) -> Result<Expr, Diag> {
        let mut e = self.not_expr()?;
        while self.is_kw("and") {
            self.next();
            let r = self.not_expr()?;
            e = Expr {
                line: e.line,
                col: e.col,
                kind: ExprKind::And(Box::new(e), Box::new(r)),
            };
        }
        Ok(e)
    }

    fn not_expr(&mut self) -> Result<Expr, Diag> {
        if self.is_kw("not") {
            let t = self.next();
            let e = self.not_expr()?;
            return Ok(Expr {
                line: t.line,
                col: t.col,
                kind: ExprKind::Not(Box::new(e)),
            });
        }
        self.comparison()
    }

    fn cmp_op(&mut self) -> Option<CmpOp> {
        let op = match &self.peek().tok {
            Tok::Op("==") => CmpOp::Eq,
            Tok::Op("!=") => CmpOp::Ne,
            Tok::Op("<") => CmpOp::Lt,
            Tok::Op("<=") => CmpOp::Le,
            Tok::Op(">") => CmpOp::Gt,
            Tok::Op(">=") => CmpOp::Ge,
            Tok::Name(n) if n == "in" => CmpOp::In,
            Tok::Name(n) if n == "is" => {
                // `x is None` / `x is not None`
                self.next();
                if self.is_kw("not") {
                    self.next();
                    return Some(CmpOp::Ne);
                }
                return Some(CmpOp::Eq);
            }
            Tok::Name(n) if n == "not" => {
                if matches!(self.toks.get(self.pos + 1).map(|t| &t.tok), Some(Tok::Name(m)) if m == "in")
                {
                    self.next();
                    CmpOp::NotIn
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        self.next();
        Some(op)
    }

    fn comparison(&mut self) -> Result<Expr, Diag> {
        let e = self.arith()?;
        let at = self.peek().clone();
        if let Some(op) = self.cmp_op() {
            let r = self.arith()?;
            let out = Expr {
                line: e.line,
                col: e.col,
                kind: ExprKind::Cmp(op, Box::new(e), Box::new(r)),
            };
            let save = self.pos;
            if self.cmp_op().is_some() {
                self.pos = save;
                return Err(diag(
                    at.line,
                    at.col,
                    "chained comparisons are not supported; combine them with `and`",
                ));
            }
            return Ok(out);
        }
        Ok(e)
    }

    fn arith(&mut self) -> Result<Expr, Diag> {
        let mut e = self.term()?;
        loop {
            let op = if self.is_op("+") {
                BinOp::Add
            } else if self.is_op("-") {
                BinOp::Sub
            } else {
                break;
            };
            self.next();
            let r = self.term()?;
            e = Expr {
                line: e.line,
                col: e.col,
                kind: ExprKind::Bin(op, Box::new(e), Box::new(r)),
            };
        }
        Ok(e)
    }

    fn term(&mut self) -> Result<Expr, Diag> {
        let mut e = self.unary()?;
        loop {
            let op = if self.is_op("*") {
                BinOp::Mul
            } else if self.is_op("//") {
                BinOp::FloorDiv
            } else if self.is_op("%") {
                BinOp::Mod
            } else {
                break;
            };
            self.next();
            let r = self.unary()?;
            e = Expr {
                line: e.line,
                col: e.col,
                kind: ExprKind::Bin(op, Box::new(e), Box::new(r)),
            };
        }
        Ok(e)
    }

    fn unary(&mut self) -> Result<Expr, Diag> {
        if self.is_op("-") {
            let t = self.next();
            let e = self.unary()?;
            return Ok(Expr {
                line: t.line,
                col: t.col,
                kind: ExprKind::Neg(Box::new(e)),
            });
        }
        if self.is_op("**") {
            return Err(self.err_here("powers (**) are not supported"));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, Diag> {
        let mut e = self.atom()?;
        loop {
            if self.is_op("(") {
                self.next();
                let mut args = vec![];
                let mut kwargs = vec![];
                while !self.is_op(")") {
                    // keyword argument?
                    if let Tok::Name(n) = &self.peek().tok
                        && matches!(
                            self.toks.get(self.pos + 1).map(|t| &t.tok),
                            Some(Tok::Op("="))
                        )
                    {
                        let n = n.clone();
                        self.next();
                        self.next();
                        kwargs.push((n, self.expr()?));
                    } else {
                        if !kwargs.is_empty() {
                            return Err(self.err_here(
                                "positional arguments must come before keyword arguments",
                            ));
                        }
                        if self.is_op("*") {
                            return Err(self.err_here("*args is not supported"));
                        }
                        args.push(self.expr()?);
                    }
                    if self.is_op(",") {
                        self.next();
                    } else {
                        break;
                    }
                }
                self.expect_op(")", "to close the call")?;
                e = Expr {
                    line: e.line,
                    col: e.col,
                    kind: ExprKind::Call {
                        func: Box::new(e),
                        args,
                        kwargs,
                    },
                };
            } else if self.is_op(".") {
                self.next();
                let (name, _, _) = self.expect_name("a name after `.`")?;
                e = Expr {
                    line: e.line,
                    col: e.col,
                    kind: ExprKind::Attr(Box::new(e), name),
                };
            } else if self.is_op("[") {
                let t = self.next();
                if self.is_op(":") {
                    return Err(diag(t.line, t.col, "slices are not supported"));
                }
                let i = self.expr()?;
                if self.is_op(":") {
                    return Err(diag(t.line, t.col, "slices are not supported"));
                }
                self.expect_op("]", "to close the index")?;
                e = Expr {
                    line: e.line,
                    col: e.col,
                    kind: ExprKind::Index(Box::new(e), Box::new(i)),
                };
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn atom(&mut self) -> Result<Expr, Diag> {
        let t = self.next();
        let (line, col) = (t.line, t.col);
        let kind = match t.tok {
            Tok::Int(n) => ExprKind::Int(n),
            Tok::Str(s) => {
                let mut s = s;
                // adjacent string literals join
                while let Tok::Str(more) = &self.peek().tok {
                    s.push_str(more);
                    self.next();
                }
                ExprKind::Str(s)
            }
            Tok::Name(n) => match n.as_str() {
                "True" => ExprKind::Bool(true),
                "False" => ExprKind::Bool(false),
                "None" => ExprKind::None,
                "lambda" => {
                    return Err(diag(
                        line,
                        col,
                        "lambda is not supported; define a function with def",
                    ));
                }
                _ if is_keyword(&n) => return Err(diag(line, col, format!("unexpected `{n}`"))),
                _ => ExprKind::Name(n),
            },
            Tok::Op("(") => {
                let e = self.expr()?;
                if self.is_op(",") {
                    return Err(self.err_here("tuples are not supported; use a list [a, b]"));
                }
                self.expect_op(")", "to close the parenthesis")?;
                return Ok(e);
            }
            Tok::Op("[") => {
                if self.is_op("]") {
                    self.next();
                    ExprKind::List(vec![])
                } else {
                    let first = self.expr()?;
                    if self.is_kw("for") {
                        self.next();
                        let (var, _, _) =
                            self.expect_name("a loop variable in the list comprehension")?;
                        if !self.is_kw("in") {
                            return Err(self.err_here("expected `in` in the list comprehension"));
                        }
                        self.next();
                        let iter = self.or_expr()?;
                        let cond = if self.is_kw("if") {
                            self.next();
                            Some(Box::new(self.or_expr()?))
                        } else {
                            None
                        };
                        if self.is_kw("for") {
                            return Err(self.err_here(
                                "nested comprehensions are not supported; use a for loop",
                            ));
                        }
                        self.expect_op("]", "to close the list comprehension")?;
                        ExprKind::ListComp {
                            elt: Box::new(first),
                            var,
                            iter: Box::new(iter),
                            cond,
                        }
                    } else {
                        let mut items = vec![first];
                        while self.is_op(",") {
                            self.next();
                            if self.is_op("]") {
                                break;
                            }
                            items.push(self.expr()?);
                        }
                        self.expect_op("]", "to close the list")?;
                        ExprKind::List(items)
                    }
                }
            }
            Tok::Op("{") => {
                return Err(diag(
                    line,
                    col,
                    "dictionaries and sets are not supported; use lists",
                ));
            }
            Tok::Newline | Tok::Eof => return Err(diag(line, col, "the line ends too early")),
            Tok::Indent | Tok::Dedent => return Err(diag(line, col, "unexpected indentation")),
            Tok::Op(o) => return Err(diag(line, col, format!("unexpected `{o}`"))),
        };
        Ok(Expr { kind, line, col })
    }
}

pub fn is_keyword(n: &str) -> bool {
    matches!(
        n,
        "if" | "elif"
            | "else"
            | "for"
            | "in"
            | "def"
            | "return"
            | "break"
            | "continue"
            | "pass"
            | "and"
            | "or"
            | "not"
            | "is"
            | "True"
            | "False"
            | "None"
            | "expect"
            | "while"
            | "import"
            | "from"
            | "class"
            | "lambda"
            | "try"
            | "except"
            | "finally"
            | "with"
            | "global"
            | "nonlocal"
            | "yield"
            | "async"
            | "await"
            | "del"
            | "assert"
            | "raise"
    )
}

pub fn parse(src: &str) -> Result<Program, Diag> {
    let toks = lex(src)?;
    Parser { toks, pos: 0 }.program()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_loops_branches_functions_and_comprehensions() {
        let src = r#"
# sort the table
def target(p):
    if p.kind == "red_block":
        return "tray-1"
    elif p.kind in ["blue_block", "green_block"]:
        return "tray-2"
    return "tray-4"

todo = [p for p in parts(at="table") if p.kind != "washer"]
for p in todo:
    move(p, target(p))
n = 0
n += 1
expect count("table") == 0, "the table is empty"
"#;
        let p = parse(src).unwrap();
        assert_eq!(p.funcs.len(), 1);
        assert_eq!(p.funcs[0].params, vec!["p"]);
        assert_eq!(p.body.len(), 5);
        assert!(matches!(p.body[1].kind, StmtKind::For(..)));
        assert!(matches!(p.body[4].kind, StmtKind::Expect(_, ref d) if d == "the table is empty"));
    }

    #[test]
    fn unsupported_python_gets_a_helpful_message_with_a_line() {
        for (src, want, line) in [
            (
                "x = 1\nwhile x < 3:\n    x += 1\n",
                "while loops are not allowed",
                2,
            ),
            ("say(f\"hi {x}\")\n", "f-strings", 1),
            ("import os\n", "imports are not available", 1),
            ("a = {1: 2}\n", "dictionaries", 1),
            ("x = 1 < 2 < 3\n", "chained comparisons", 1),
            ("if True:\nx = 1\n", "indented block", 2),
            ("x = y[1:2]\n", "slices", 1),
        ] {
            let d = parse(src).unwrap_err();
            assert!(d.message.contains(want), "{src:?}: {}", d.message);
            assert_eq!(d.line, line, "{src:?}");
        }
    }
}
