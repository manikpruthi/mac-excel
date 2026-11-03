//! FILTERXML: XML content with an XPath subset, as Excel's Windows-only function does it.
//!
//! Excel (MSXML underneath) returns the text of every node an XPath selects from one document,
//! spilled as a vertical array; a range of documents gives one value per input cell — the first
//! match. Invalid XML, an unbound namespace prefix, an XPath that does not parse or matches
//! nothing, and an XPath over 1024 characters all give `#VALUE!`. Values that look like numbers
//! become numbers, and each value is trimmed. See plan/excel/filterxml.md (E2, E3, E5, E6).

use std::collections::HashSet;

use gridcraft_core::{CellError, Value};
use quick_xml::Reader;
use quick_xml::events::Event;

use crate::util::{A, R, array_val, text, text_val};
use crate::{Arg, Ctx, FnSpec};

/// Documents one call reads at most: a range of documents is per-cell work, and a hostile range
/// must not become unbounded work.
const MAX_DOCUMENTS: usize = 100_000;
/// Elements one document may hold. The XML text is bounded by the 32,767-character cell text
/// limit, but a document of `<a/>` tags is 4 bytes each, so the count is bounded apart from the
/// bytes.
const MAX_ELEMENTS: usize = 500_000;
/// Nesting depth: hostile input must not overflow the stack.
const MAX_DEPTH: usize = 10_000;
/// Bytes of XML one document may hold.
const MAX_XML: usize = 8 * 1024 * 1024;
/// Excel's XPath parameter limit (WorksheetFunction.FilterXML).
const MAX_XPATH: usize = 1024;

pub(super) fn specs() -> Vec<FnSpec> {
    vec![f!("FILTERXML", 2, 2, Web, A, "FILTERXML(xml, xpath)", "Extracts data from XML content with an XPath expression.", filterxml)]
}

// ---------------------------------------------------------------------------------------------
// Document

/// One element: its qualified name (prefix included, matched literally), attributes, children
/// and its own text.
struct Node {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<usize>,
    text: String,
}

struct Doc {
    nodes: Vec<Node>,
}

impl Doc {
    /// An element's string-value: its text and all its descendants', concatenated.
    fn string_value(&self, n: usize) -> String {
        let mut out = String::new();
        self.collect_text(n, &mut out);
        out.trim().to_string()
    }
    fn collect_text(&self, n: usize, out: &mut String) {
        let Some(node) = self.nodes.get(n) else { return };
        out.push_str(&node.text);
        for &c in &node.children {
            self.collect_text(c, out);
        }
    }
}

/// Whether a qualified name's prefix is bound (`xml` always is).
fn prefix_ok(name: &str, prefixes: &HashSet<String>) -> bool {
    match name.split_once(':') {
        Some((p, _)) => p == "xml" || prefixes.contains(p),
        None => true,
    }
}

/// Parses one XML document. `None` for anything Excel would refuse: not well-formed, content
/// outside the root, or a namespace prefix that is not declared.
fn parse_doc(xml: &str) -> Option<Doc> {
    if xml.len() > MAX_XML {
        return None;
    }
    let mut r = Reader::from_str(xml);
    let mut nodes: Vec<Node> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut prefixes: HashSet<String> = HashSet::new();
    let mut root_seen = false;
    loop {
        let ev = r.read_event().ok()?;
        let empty = matches!(ev, Event::Empty(_));
        match ev {
            Event::Start(e) | Event::Empty(e) => {
                if root_seen && stack.is_empty() {
                    return None; // content after the root closed
                }
                root_seen = true;
                let name = String::from_utf8(e.name().as_ref().to_vec()).ok()?;
                if !prefix_ok(&name, &prefixes) {
                    return None;
                }
                let mut attrs = Vec::new();
                for a in e.attributes().with_checks(false) {
                    let a = a.ok()?;
                    let an = String::from_utf8(a.key.as_ref().to_vec()).ok()?;
                    if an == "xmlns" {
                        continue;
                    }
                    if let Some(p) = an.strip_prefix("xmlns:") {
                        prefixes.insert(p.to_string());
                        continue;
                    }
                    if !prefix_ok(&an, &prefixes) {
                        return None;
                    }
                    let v = a.normalized_value(quick_xml::XmlVersion::Explicit1_0).ok()?.to_string();
                    attrs.push((an, v));
                }
                let i = nodes.len();
                if i >= MAX_ELEMENTS {
                    return None;
                }
                nodes.push(Node { name, attrs, children: Vec::new(), text: String::new() });
                if let Some(&p) = stack.last() {
                    nodes[p].children.push(i);
                }
                if !empty {
                    if stack.len() >= MAX_DEPTH {
                        return None;
                    }
                    stack.push(i);
                }
            }
            Event::Text(t) => {
                let s = t.xml_content(quick_xml::XmlVersion::Explicit1_0).ok()?;
                let Some(&p) = stack.last() else {
                    if s.trim().is_empty() {
                        continue;
                    }
                    return None;
                };
                nodes[p].text.push_str(&s);
            }
            Event::CData(t) => {
                let s = t.decode().ok()?;
                let Some(&p) = stack.last() else {
                    if s.trim().is_empty() {
                        continue;
                    }
                    return None;
                };
                nodes[p].text.push_str(&s);
            }
            Event::End(_) => {
                stack.pop()?; // an end tag without a start
            }
            Event::Eof => break,
            // Comments, declarations, processing instructions and DTDs carry no values. A
            // reference Excel's MSXML resolves (predefined or numeric) is its character; anything
            // else is an undeclared entity, which Excel refuses with #VALUE!.
            Event::GeneralRef(e) => {
                let &p = stack.last()?;
                let n = e.decode().ok()?;
                let c = if e.is_char_ref() {
                    let h = n.strip_prefix('#').unwrap_or(&n);
                    let code = if let Some(x) = h.strip_prefix('x') { u32::from_str_radix(x, 16) } else { h.parse::<u32>() }.ok()?;
                    char::from_u32(code)
                } else {
                    match n.as_ref() {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        _ => None,
                    }
                };
                nodes[p].text.push(c?);
            }
            _ => {}
        }
    }
    if !root_seen || !stack.is_empty() {
        return None;
    }
    Some(Doc { nodes })
}

// ---------------------------------------------------------------------------------------------
// XPath

/// A node test: an element name, any element, the text nodes, or an attribute (`@name`, `@*`).
enum Test {
    Name(String),
    Any,
    Text,
    Attr(Option<String>),
}

#[derive(Clone, Copy, PartialEq)]
enum Cmp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

/// The right side of a predicate comparison.
enum Side {
    Str(String),
    Num(f64),
    Attr(String),
}

enum Pred {
    Position(f64),
    Last(f64),
    AttrHas(String),
    AttrCmp(String, Cmp, Side),
    ChildHas(String),
    ChildCmp(String, Cmp, Side),
}

struct Step {
    /// `//` before this step: the candidates are the context and its descendants first.
    desc: bool,
    test: Test,
    preds: Vec<Pred>,
}

struct Path {
    steps: Vec<Step>,
}

enum Call {
    Count(Path),
    Sum(Path),
    String(Path),
}

enum Expr {
    Path(Vec<Path>),
    Call(Call),
}

// ---------------------------------------------------------------- tokens

#[derive(Debug, PartialEq, Clone)]
enum Tok {
    Slash,
    SlashSlash,
    Name(String),
    Star,
    At,
    LB,
    RB,
    LP,
    RP,
    Comma,
    Bar,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Minus,
    Plus,
    Num(f64),
    Str(String),
}

fn name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c.is_alphabetic()
}
fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-') || c.is_alphanumeric()
}

fn tokenize(x: &str) -> Option<Vec<Tok>> {
    let c: Vec<char> = x.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < c.len() {
        let ch = c[i];
        i += 1;
        match ch {
            ' ' | '\t' | '\n' | '\r' => {}
            '/' => {
                if c.get(i) == Some(&'/') {
                    i += 1;
                    out.push(Tok::SlashSlash);
                } else {
                    out.push(Tok::Slash);
                }
            }
            '@' => out.push(Tok::At),
            '[' => out.push(Tok::LB),
            ']' => out.push(Tok::RB),
            '(' => out.push(Tok::LP),
            ')' => out.push(Tok::RP),
            ',' => out.push(Tok::Comma),
            '|' => out.push(Tok::Bar),
            '*' => out.push(Tok::Star),
            '=' => out.push(Tok::Eq),
            '!' => {
                if c.get(i) == Some(&'=') {
                    i += 1;
                    out.push(Tok::Ne);
                } else {
                    return None;
                }
            }
            '<' => {
                if c.get(i) == Some(&'=') {
                    i += 1;
                    out.push(Tok::Le);
                } else {
                    out.push(Tok::Lt);
                }
            }
            '>' => {
                if c.get(i) == Some(&'=') {
                    i += 1;
                    out.push(Tok::Ge);
                } else {
                    out.push(Tok::Gt);
                }
            }
            '-' => out.push(Tok::Minus),
            '+' => out.push(Tok::Plus),
            '\'' | '"' => {
                let mut s = String::new();
                let mut closed = false;
                while i < c.len() {
                    if c[i] == ch {
                        i += 1;
                        closed = true;
                        break;
                    }
                    s.push(c[i]);
                    i += 1;
                }
                if !closed {
                    return None;
                }
                out.push(Tok::Str(s));
            }
            d if d.is_ascii_digit() || (d == '.' && c.get(i).is_some_and(|n| n.is_ascii_digit())) => {
                let mut s = String::new();
                s.push(d);
                while i < c.len() && (c[i].is_ascii_digit() || c[i] == '.') {
                    s.push(c[i]);
                    i += 1;
                }
                if c.get(i) == Some(&'e') || c.get(i) == Some(&'E') {
                    let mut e = String::from("e");
                    i += 1;
                    if c.get(i) == Some(&'-') || c.get(i) == Some(&'+') {
                        e.push(c[i]);
                        i += 1;
                    }
                    if c.get(i).is_some_and(|d| d.is_ascii_digit()) {
                        while i < c.len() && c[i].is_ascii_digit() {
                            e.push(c[i]);
                            i += 1;
                        }
                        s.push_str(&e);
                    } else {
                        return None;
                    }
                }
                out.push(Tok::Num(s.parse().ok()?));
            }
            n if name_start(n) => {
                let mut s = String::new();
                s.push(n);
                while i < c.len() && name_char(c[i]) {
                    s.push(c[i]);
                    i += 1;
                }
                out.push(Tok::Name(s));
            }
            _ => return None,
        }
    }
    Some(out)
}

// ---------------------------------------------------------------- parser

struct P {
    t: Vec<Tok>,
    i: usize,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn at(&self, k: usize) -> Option<&Tok> {
        self.t.get(self.i + k)
    }
    fn bump(&mut self) -> Option<Tok> {
        let t = self.t.get(self.i).cloned();
        if t.is_some() {
            self.i += 1;
        }
        t
    }
    fn eat(&mut self, k: &Tok) -> bool {
        if self.peek() == Some(k) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expr(&mut self) -> Option<Expr> {
        // count(…), sum(…), string(…): the function forms Excel's own examples use.
        if let (Some(Tok::Name(n)), Some(Tok::LP)) = (self.peek(), self.at(1))
            && matches!(n.as_str(), "count" | "sum" | "string")
        {
            let name = n.clone();
            self.bump();
            self.bump();
            let path = self.path()?;
            if !self.eat(&Tok::RP) || self.peek().is_some() {
                return None;
            }
            let call = match name.as_str() {
                "count" => Call::Count(path),
                "sum" => Call::Sum(path),
                _ => Call::String(path),
            };
            return Some(Expr::Call(call));
        }
        let mut paths = vec![self.path()?];
        while self.eat(&Tok::Bar) {
            paths.push(self.path()?);
        }
        if self.peek().is_some() {
            return None;
        }
        Some(Expr::Path(paths))
    }

    fn path(&mut self) -> Option<Path> {
        // Leading `//` or `/`: the context is the document either way, but `//` makes the first
        // step select descendants.
        let desc_first = self.eat(&Tok::SlashSlash) || (self.eat(&Tok::Slash) && self.eat(&Tok::SlashSlash));
        let mut steps = vec![self.step(desc_first)?];
        loop {
            if self.eat(&Tok::SlashSlash) {
                steps.push(self.step(true)?);
            } else if self.eat(&Tok::Slash) {
                steps.push(self.step(false)?);
            } else {
                break;
            }
        }
        // Attributes and text are leaf selections: they may only be the last step, and they
        // take no predicates.
        for s in &steps[..steps.len().saturating_sub(1)] {
            if matches!(s.test, Test::Text | Test::Attr(_)) {
                return None;
            }
        }
        if let Some(last) = steps.last()
            && matches!(last.test, Test::Text | Test::Attr(_))
            && !last.preds.is_empty()
        {
            return None;
        }
        Some(Path { steps })
    }

    fn step(&mut self, desc: bool) -> Option<Step> {
        let test = match self.peek() {
            Some(Tok::Star) => {
                self.bump();
                Test::Any
            }
            Some(Tok::At) => {
                self.bump();
                match self.bump() {
                    Some(Tok::Star) => Test::Attr(None),
                    Some(Tok::Name(n)) => Test::Attr(Some(n)),
                    _ => return None,
                }
            }
            Some(Tok::Name(n)) if self.at(1) == Some(&Tok::LP) => {
                let n = n.clone();
                if n != "text" && n != "node" {
                    return None;
                }
                self.bump();
                self.bump();
                if !self.eat(&Tok::RP) {
                    return None;
                }
                if n == "text" { Test::Text } else { Test::Any }
            }
            Some(Tok::Name(n)) => {
                let n = n.clone();
                self.bump();
                Test::Name(n)
            }
            _ => return None,
        };
        let mut preds = Vec::new();
        while self.peek() == Some(&Tok::LB) {
            self.bump();
            preds.push(self.pred()?);
            if !self.eat(&Tok::RB) {
                return None;
            }
        }
        Some(Step { desc, test, preds })
    }

    fn pred(&mut self) -> Option<Pred> {
        match self.bump() {
            Some(Tok::Num(n)) => Some(Pred::Position(n)),
            Some(Tok::Minus) => match self.bump() {
                Some(Tok::Num(n)) => Some(Pred::Position(-n)),
                _ => None,
            },
            Some(Tok::Plus) => match self.bump() {
                Some(Tok::Num(n)) => Some(Pred::Position(n)),
                _ => None,
            },
            Some(Tok::Name(n)) if n == "last" && self.at(0) == Some(&Tok::LP) => {
                self.bump();
                if !self.eat(&Tok::RP) {
                    return None;
                }
                let mut off = 0.0;
                match self.peek() {
                    Some(Tok::Minus) => {
                        self.bump();
                        off = -self.bump()?.num()?;
                    }
                    Some(Tok::Plus) => {
                        self.bump();
                        off = self.bump()?.num()?;
                    }
                    _ => {}
                }
                Some(Pred::Last(off))
            }
            Some(Tok::Name(n)) if n == "position" && self.at(0) == Some(&Tok::LP) => {
                self.bump();
                if !self.eat(&Tok::RP) || !self.eat(&Tok::Eq) {
                    return None;
                }
                Some(Pred::Position(self.bump()?.num()?))
            }
            Some(Tok::At) => {
                let a = self.bump()?.name()?;
                if let Some(c) = self.cmp() { Some(Pred::AttrCmp(a, c, self.side()?)) } else { Some(Pred::AttrHas(a)) }
            }
            Some(Tok::Name(n)) => {
                if let Some(c) = self.cmp() {
                    Some(Pred::ChildCmp(n, c, self.side()?))
                } else {
                    Some(Pred::ChildHas(n))
                }
            }
            _ => None,
        }
    }

    fn cmp(&mut self) -> Option<Cmp> {
        match self.peek() {
            Some(Tok::Eq) => {
                self.bump();
                Some(Cmp::Eq)
            }
            Some(Tok::Ne) => {
                self.bump();
                Some(Cmp::Ne)
            }
            Some(Tok::Lt) => {
                self.bump();
                Some(Cmp::Lt)
            }
            Some(Tok::Le) => {
                self.bump();
                Some(Cmp::Le)
            }
            Some(Tok::Gt) => {
                self.bump();
                Some(Cmp::Gt)
            }
            Some(Tok::Ge) => {
                self.bump();
                Some(Cmp::Ge)
            }
            _ => None,
        }
    }

    fn side(&mut self) -> Option<Side> {
        match self.peek() {
            Some(Tok::Minus) => {
                self.bump();
                match self.bump() {
                    Some(Tok::Num(n)) => Some(Side::Num(-n)),
                    _ => None,
                }
            }
            Some(Tok::Plus) => {
                self.bump();
                match self.bump() {
                    Some(Tok::Num(n)) => Some(Side::Num(n)),
                    _ => None,
                }
            }
            _ => match self.bump() {
                Some(Tok::Str(s)) => Some(Side::Str(s)),
                Some(Tok::Num(n)) => Some(Side::Num(n)),
                Some(Tok::At) => match self.bump() {
                    Some(Tok::Name(a)) => Some(Side::Attr(a)),
                    _ => None,
                },
                Some(Tok::Name(n)) => Some(Side::Str(n)),
                _ => None,
            },
        }
    }
}

trait TokNum {
    fn num(self) -> Option<f64>;
    fn name(self) -> Option<String>;
}
impl TokNum for Tok {
    fn num(self) -> Option<f64> {
        match self {
            Tok::Num(n) => Some(n),
            _ => None,
        }
    }
    fn name(self) -> Option<String> {
        match self {
            Tok::Name(n) => Some(n),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------- evaluation

/// A selected node: an element, one of its attributes, or its text. The element index keeps
/// duplicates apart when a union selects the same value twice.
enum Sel {
    Node(usize),
    Attr { elem: usize, name: String, value: String },
    Text { elem: usize, value: String },
}

enum Outcome {
    Nodes(Vec<Sel>),
    Num(f64),
    Text(String),
}

/// The document sentinel: the parent of the root, so relative paths start there.
const DOC: usize = usize::MAX;

fn eval(doc: &Doc, e: &Expr) -> Outcome {
    match e {
        Expr::Call(Call::Count(p)) => Outcome::Num(eval_path(doc, p).len() as f64),
        Expr::Call(Call::Sum(p)) => {
            let mut s = 0.0;
            for n in eval_path(doc, p) {
                s += doc.string_value(n).trim().parse::<f64>().unwrap_or(f64::NAN);
            }
            Outcome::Num(s)
        }
        Expr::Call(Call::String(p)) => Outcome::Text(eval_path(doc, p).first().map(|n| doc.string_value(*n)).unwrap_or_default()),
        Expr::Path(ps) => {
            let mut sels = Vec::new();
            for p in ps {
                sels.extend(eval_path_sels(doc, p));
            }
            // A union holds each node once, in document order.
            let mut seen = HashSet::new();
            sels.retain(|s| {
                let key = match s {
                    Sel::Node(n) => format!("n{n}"),
                    Sel::Attr { elem, name, .. } => format!("a{elem}:{name}"),
                    Sel::Text { elem, .. } => format!("t{elem}"),
                };
                seen.insert(key)
            });
            Outcome::Nodes(sels)
        }
    }
}

fn children_of(doc: &Doc, n: usize) -> Vec<usize> {
    if n == DOC { doc.nodes.first().map(|_| vec![0]).unwrap_or_default() } else { doc.nodes.get(n).map(|x| x.children.clone()).unwrap_or_default() }
}

/// The context nodes themselves plus every descendant, in document order.
fn descendants_or_self(doc: &Doc, ctx: &[usize]) -> Vec<usize> {
    if ctx.contains(&DOC) {
        return (0..doc.nodes.len()).collect();
    }
    let mut out = Vec::new();
    let mut work: Vec<usize> = ctx.to_vec();
    let mut i = 0;
    while i < work.len() {
        let n = work[i];
        i += 1;
        out.push(n);
        if let Some(c) = doc.nodes.get(n) {
            work.extend_from_slice(&c.children);
        }
    }
    out
}

fn matches_test(doc: &Doc, n: usize, test: &Test) -> bool {
    match test {
        Test::Name(x) => doc.nodes.get(n).is_some_and(|node| node.name == *x),
        Test::Any => doc.nodes.get(n).is_some(),
        Test::Text | Test::Attr(_) => false,
    }
}

/// One step over element contexts: `//` takes the context and its descendants, `/` takes the
/// children, then the node test and predicates pick from them.
fn apply_step(doc: &Doc, ctx: &[usize], s: &Step) -> Vec<usize> {
    let base = if s.desc { descendants_or_self(doc, ctx) } else { ctx.iter().flat_map(|n| children_of(doc, *n)).collect() };
    let matched: Vec<usize> = base.into_iter().filter(|n| matches_test(doc, *n, &s.test)).collect();
    apply_preds(doc, matched, &s.preds)
}

fn eval_path(doc: &Doc, p: &Path) -> Vec<usize> {
    eval_path_sels(doc, p)
        .into_iter()
        .filter_map(|s| match s {
            Sel::Node(n) => Some(n),
            _ => None,
        })
        .collect()
}

fn eval_path_sels(doc: &Doc, p: &Path) -> Vec<Sel> {
    let Some((last, elem_steps)) = p.steps.split_last() else { return Vec::new() };
    let mut ctx: Vec<usize> = vec![DOC];
    for s in elem_steps {
        ctx = apply_step(doc, &ctx, s);
    }
    if last.desc {
        ctx = descendants_or_self(doc, &ctx);
    }
    match &last.test {
        Test::Name(_) | Test::Any => {
            let base = if last.desc { ctx } else { ctx.iter().flat_map(|n| children_of(doc, *n)).collect() };
            let matched: Vec<usize> = base.into_iter().filter(|n| matches_test(doc, *n, &last.test)).collect();
            apply_preds(doc, matched, &last.preds).into_iter().map(Sel::Node).collect()
        }
        Test::Text => ctx
            .into_iter()
            .filter(|n| doc.nodes.get(*n).is_some_and(|x| !x.text.is_empty()))
            .map(|n| Sel::Text { elem: n, value: doc.nodes[n].text.trim().to_string() })
            .collect(),
        Test::Attr(a) => {
            let mut out = Vec::new();
            for n in ctx {
                let Some(node) = doc.nodes.get(n) else { continue };
                match a {
                    Some(name) => {
                        if let Some((_, v)) = node.attrs.iter().find(|(k, _)| k == name) {
                            out.push(Sel::Attr { elem: n, name: name.clone(), value: v.clone() });
                        }
                    }
                    None => {
                        for (k, v) in &node.attrs {
                            out.push(Sel::Attr { elem: n, name: k.clone(), value: v.clone() });
                        }
                    }
                }
            }
            out
        }
    }
}

fn apply_preds(doc: &Doc, candidates: Vec<usize>, preds: &[Pred]) -> Vec<usize> {
    let mut out = candidates;
    for pr in preds {
        let size = out.len();
        out = out.into_iter().enumerate().filter(|(pos, n)| pred_ok(doc, *n, pr, (*pos + 1) as f64, size as f64)).map(|(_, n)| n).collect();
    }
    out
}

fn side_value(doc: &Doc, n: usize, s: &Side) -> Option<String> {
    match s {
        Side::Str(x) => Some(x.clone()),
        Side::Num(x) => Some(x.to_string()),
        Side::Attr(a) => doc.nodes.get(n)?.attrs.iter().find(|(k, _)| k == a).map(|(_, v)| v.clone()),
    }
}

fn pred_ok(doc: &Doc, n: usize, p: &Pred, pos: f64, size: f64) -> bool {
    match p {
        Pred::Position(k) => pos == *k,
        Pred::Last(off) => pos == size + off,
        Pred::AttrHas(a) => doc.nodes.get(n).is_some_and(|x| x.attrs.iter().any(|(k, _)| k == a)),
        Pred::AttrCmp(a, c, s) => {
            let Some(left) = doc.nodes.get(n).and_then(|x| x.attrs.iter().find(|(k, _)| k == a).map(|(_, v)| v.clone())) else {
                return false;
            };
            compare(&left, c, s, doc, n)
        }
        Pred::ChildHas(t) => doc.nodes.get(n).is_some_and(|x| x.children.iter().any(|c| doc.nodes.get(*c).is_some_and(|y| y.name == *t))),
        Pred::ChildCmp(t, c, s) => {
            let Some(idx) = doc.nodes.get(n).and_then(|x| x.children.iter().copied().find(|c| doc.nodes.get(*c).is_some_and(|y| y.name == *t)))
            else {
                return false;
            };
            let left = doc.string_value(idx);
            compare(&left, c, s, doc, n)
        }
    }
}

fn compare(left: &str, c: &Cmp, s: &Side, doc: &Doc, n: usize) -> bool {
    let Some(right) = side_value(doc, n, s) else { return false };
    match (left.trim().parse::<f64>(), right.trim().parse::<f64>()) {
        (Ok(l), Ok(r)) if l.is_finite() && r.is_finite() => match c {
            Cmp::Eq => l == r,
            Cmp::Ne => l != r,
            Cmp::Gt => l > r,
            Cmp::Ge => l >= r,
            Cmp::Lt => l < r,
            Cmp::Le => l <= r,
        },
        _ => match c {
            Cmp::Eq => left == right,
            Cmp::Ne => left != right,
            // Relational comparisons need numbers on both sides; anything else matches nothing.
            _ => false,
        },
    }
}

// ---------------------------------------------------------------- the function

/// The value of one selection: an element's string-value, an attribute's value, or text.
/// Anything that looks like a number becomes a number, as Excel converts it (E5).
fn sel_value(doc: &Doc, s: &Sel) -> Value {
    let t = match s {
        Sel::Node(n) => doc.string_value(*n),
        Sel::Attr { value, .. } => value.clone(),
        Sel::Text { value, .. } => value.clone(),
    };
    let t = t.trim();
    if let Ok(n) = t.parse::<f64>()
        && n.is_finite()
    {
        Value::Number(n)
    } else {
        Value::text(t)
    }
}

/// Evaluates the expression over one document: the first match when the document is one cell of
/// a range argument, all matches otherwise.
fn doc_value(v: &Value, expr: &Expr, first_only: bool) -> Result<Value, ()> {
    let s = v.to_text().map_err(|_| ())?;
    let doc = parse_doc(&s).ok_or(())?;
    match eval(&doc, expr) {
        Outcome::Num(n) if n.is_finite() => Ok(Value::Number(n)),
        Outcome::Num(_) => Err(()),
        Outcome::Text(t) => text_val(t).map_err(|_| ()),
        Outcome::Nodes(sels) => {
            if sels.is_empty() {
                return Err(());
            }
            if first_only || sels.len() == 1 {
                Ok(sel_value(&doc, &sels[0]))
            } else {
                array_val(sels.len(), 1, sels.iter().map(|s| sel_value(&doc, s)).collect()).map_err(|_| ())
            }
        }
    }
}

pub(super) fn filterxml(a: &[Arg], _c: &mut dyn Ctx) -> R<Value> {
    let xpath = text(a, 1)?;
    if xpath.chars().count() > MAX_XPATH {
        return Err(CellError::Value);
    }
    let expr = parse_xpath(&xpath).ok_or(CellError::Value)?;
    let xml = a.first().map(|x| &x.value).unwrap_or(&Value::Empty);
    match xml {
        Value::Error(e) => Err(*e),
        // A range of documents: one value per input cell, the first match, errors included.
        Value::Array(arr) => {
            if arr.data.len() > MAX_DOCUMENTS {
                return Err(CellError::Value);
            }
            let out = arr.data.iter().map(|v| doc_value(v, &expr, true).unwrap_or(Value::Error(CellError::Value))).collect();
            array_val(arr.rows, arr.cols, out)
        }
        v => doc_value(v, &expr, false).map_err(|()| CellError::Value),
    }
}

fn parse_xpath(x: &str) -> Option<Expr> {
    let mut p = P { t: tokenize(x)?, i: 0 };
    p.expr()
}

#[cfg(test)]
mod tests {
    use gridcraft_core::{CellError, Value};

    use crate::util::testutil::*;

    fn fx(xml: &str, xpath: &str) -> Value {
        ev("FILTERXML", vec![t(xml), t(xpath)])
    }

    #[test]
    fn element_attribute_and_path() {
        is_text(fx("<a><b>x</b></a>", "//b"), "x");
        is_text(fx("<a><b>1 &amp; 2</b></a>", "//b"), "1 & 2");
        is_text(fx("<a><b>&#65;&#x42;</b></a>", "//b"), "AB");
        is_text(fx("<a><b>x</b></a>", "a/b"), "x");
        is_text(fx("<a><b>x</b></a>", "/a/b"), "x");
        is_text(fx("<rss><rc title=\"T\" ts=\"U\"/></rss>", "//rc/@title"), "T");
        is_text(fx("<a><b>  spaced  </b></a>", "//b"), "spaced");
        // Case-sensitive tags, as Excel reads them.
        is_err(fx("<a><Title>1</Title></a>", "//title"), CellError::Value);
    }

    #[test]
    fn values_that_look_like_numbers_become_numbers() {
        close(fx("<t><n>4.50</n></t>", "//n"), 4.5);
        close(fx("<t><n>02108</n></t>", "//n"), 2108.0);
        is_text(fx("<t><n>abc</n></t>", "//n"), "abc");
    }

    #[test]
    fn several_matches_spill_vertically() {
        let v = fx("<t><r><s>1</s></r><r><s>2</s></r><r><s>3</s></r></t>", "//r/s");
        assert_eq!(rows_of(&v), vec![vec![nv(1.0)], vec![nv(2.0)], vec![nv(3.0)]], "{v:?}");
    }

    #[test]
    fn predicates() {
        let xml = "<t><r><s>a</s></r><r><s>b</s></r><r><s>c</s></r></t>";
        is_text(fx(xml, "//s[1]"), "a");
        is_text(fx(xml, "//s[2]"), "b");
        is_text(fx(xml, "//s[last()]"), "c");
        is_text(fx(xml, "//s[last()-1]"), "b");
        is_text(fx(xml, "//s[position()=3]"), "c");
        let xml = "<t><class><name>Gym</name><spots>3</spots></class><class><name>Swim</name><spots>0</spots></class></t>";
        is_text(fx(xml, "//class[spots>0]/name"), "Gym");
        is_text(fx(xml, "//class[spots=0]/name"), "Swim");
        is_text(fx(xml, "//class[spots>1]/name"), "Gym");
        let xml = "<t><item id=\"1\"><name>first</name></item><item id=\"2\"><name>second</name></item></t>";
        // Both items have the attribute: the matches spill, as Excel's dynamic arrays do.
        let v = fx(xml, "//item[@id]/name");
        assert_eq!(rows_of(&v), vec![vec![Value::from("first")], vec![Value::from("second")]], "{v:?}");
        is_text(fx(xml, "//item[@id='2']/name"), "second");
        is_text(fx(xml, "//item[@id!='1']/name"), "second");
        is_text(fx(xml, "//item[@id>'1']/name"), "second");
    }

    #[test]
    fn functions_unions_text_and_wildcards() {
        close(fx("<t><s>1</s><s>2</s><s>3</s></t>", "count(//s)"), 3.0);
        close(fx("<t><n>1.5</n><n>2.5</n></t>", "sum(//n)"), 4.0);
        is_text(fx("<a><b>hello<b>world</b></b></a>", "string(//a)"), "helloworld");
        let v = fx("<a><b>1</b><c>2</c></a>", "//b|//c");
        assert_eq!(rows_of(&v), vec![vec![nv(1.0)], vec![nv(2.0)]], "{v:?}");
        // The same value selected twice by a union is one cell.
        let v = fx("<a><b>1</b></a>", "//b|//b");
        assert_eq!(rows_of(&v), vec![vec![nv(1.0)]], "{v:?}");
        is_text(fx("<a><b>x</b></a>", "//b/text()"), "x");
        // //* matches every element; an element's value is its text and its descendants'.
        let v = fx("<a><b>1</b><c>2</c></a>", "//*");
        assert_eq!(rows_of(&v).len(), 3, "{v:?}");
    }

    #[test]
    fn errors_like_excel() {
        is_err(fx("<a><b>x</b></a>", "//c"), CellError::Value); // no match
        is_err(fx("<a>", "//a"), CellError::Value); // not well-formed
        is_err(fx("", "//a"), CellError::Value); // empty
        is_err(fx("<a><b>x</b></a>", "//["), CellError::Value); // bad xpath
        is_err(fx("<a><b>x</b></a>", "//b[last()"), CellError::Value); // unclosed predicate
        is_err(fx("<a><b>x</b></a>", "//a/@b/c"), CellError::Value); // a path through an attribute
        // An undeclared namespace prefix is refused, as Excel documents it.
        is_err(fx("<x:y>t</x:y>", "//x:y"), CellError::Value);
        is_err(fx("<a xmlns:p=\"u\"><p:b>t</p:b></a>", "//q:b"), CellError::Value);
        // XPath over 1024 characters.
        let long = format!("//{}", "a".repeat(1025));
        is_err(fx("<a><b>x</b></a>", &long), CellError::Value);
    }

    #[test]
    fn a_declared_prefix_matches_and_a_range_gives_one_value_per_cell() {
        is_text(fx("<a xmlns:p=\"u\"><p:b>t</p:b></a>", "//p:b"), "t");
        // A range of documents: one value per input cell, the first match, errors included.
        let docs = arr(vec![vec![Value::from("<t><s>1</s><s>2</s></t>")], vec![Value::from("<t><s>3</s></t>")]]);
        let v = ev("FILTERXML", vec![av(docs), t("//s")]);
        assert_eq!(rows_of(&v), vec![vec![nv(1.0)], vec![nv(3.0)]], "{v:?}");
        let bad = arr(vec![vec![Value::from("<t><s>1</s></t>")], vec![nv(5.0)]]);
        let v = ev("FILTERXML", vec![av(bad), t("//s")]);
        assert_eq!(rows_of(&v), vec![vec![nv(1.0)], vec![Value::Error(CellError::Value)]], "{v:?}");
    }
}
