//! Presentation MathML import: a compact, bounded reader that maps the common presentation
//! elements (`mi mn mo mtext ms mspace mrow mstyle mfrac msqrt mroot msub msup msubsup munder
//! mover munderover mtable mtr mtd mfenced menclose mmultiscripts semantics`) onto the math tree.
//! Unknown elements read as `mrow` (their children); annotations and phantoms are dropped.
//!
//! Unlike the OMML reader this one is fallible: text that is not a well-formed MathML `math`
//! element is an error, so a caller (paste) can fall back to plain text.

use crate::xml::{self, XEl, XNode};
use crate::{MAX_DEPTH, Math, Node, Seq, Style};

/// The MathML namespace.
pub const MATHML_NS: &str = "http://www.w3.org/1998/Math/MathML";

/// Largest MathML document read.
pub const MAX_MATHML: usize = 1 << 20;

/// Big operators that take limits and an operand when scripted.
const NARY: &[char] = &['∑', '∏', '∐', '∫', '∬', '∭', '∮', '∯', '∰', '⋃', '⋂', '⋁', '⋀', '⨁', '⨂', '⨀', '⨄'];

/// Operators that end the operand of a big operator.
const RELATIONS: &[char] = &['=', '<', '>', '≤', '≥', '≠', '≈', '≡', '∼', '≅', '∝', '→', '←', '⇒', '⇔', '⟹', ',', ';'];

const OPEN: &[char] = &['(', '[', '{', '⟨', '⌈', '⌊', '|', '‖', '〈'];
const CLOSE: &[char] = &[')', ']', '}', '⟩', '⌉', '⌋', '|', '‖', '〉'];

/// Parse a MathML `<math>` element (optionally prefixed, after an XML declaration).
pub fn from_mathml(src: &str) -> Result<Math, String> {
    let src = src.trim();
    if src.len() > MAX_MATHML {
        return Err("the MathML is too large".into());
    }
    xml::well_formed(src).map_err(|e| format!("not well-formed MathML: {e}"))?;
    let roots = xml::parse(src);
    let root = match roots.as_slice() {
        [r] => r,
        _ => return Err("not a single MathML element".into()),
    };
    if root.local() != "math" {
        return Err("the root element is not <math>".into());
    }
    let decl = if root.prefix().is_empty() { "xmlns".to_string() } else { format!("xmlns:{}", root.prefix()) };
    let ns = root.attrs.iter().find(|(k, _)| *k == decl).map(|(_, v)| v.as_str());
    match ns {
        Some(v) if v != MATHML_NS => return Err("the <math> element is not in the MathML namespace".into()),
        None if !root.prefix().is_empty() => return Err("the <math> prefix is not bound to the MathML namespace".into()),
        _ => {}
    }
    let body = row(root, 0)?;
    let display = root.attr_local("display") == Some("block") || root.attr_local("mode") == Some("display");
    let m = Math { body, para: display, jc: None, attrs: String::new() };
    if m.is_empty() {
        return Err("the MathML equation is empty".into());
    }
    Ok(m)
}

fn els(e: &XEl) -> Vec<&XEl> {
    e.els().collect()
}

/// Token text: whitespace trimmed and collapsed, as MathML renders it.
fn token(e: &XEl) -> String {
    let mut out = String::new();
    for c in &e.children {
        if let XNode::Text(t) = c {
            out.push_str(t);
        } else if let XNode::El(x) = c
            && x.local() != "mglyph"
        {
            out.push_str(&x.text());
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn single(t: &str) -> Option<char> {
    let mut it = t.chars();
    let c = it.next()?;
    it.next().is_none().then_some(c)
}

/// The operator character of an `mo` element, if it is one.
fn mo_char(e: &XEl) -> Option<char> {
    if e.local() == "mo" { single(&token(e)) } else { None }
}

fn invisible(c: char) -> bool {
    matches!(c, '\u{2061}'..='\u{2064}' | '\u{200B}')
}

fn too_deep(d: usize) -> Result<(), String> {
    if d > MAX_DEPTH { Err("the MathML is nested too deeply".into()) } else { Ok(()) }
}

/// The children of `e` read as one row.
fn row(e: &XEl, d: usize) -> Result<Seq, String> {
    seq(&els(e), d)
}

/// A row of elements: matched fences become a delimiter, `mi ⁡ x` a function, a scripted big
/// operator takes what follows up to the next relation as its operand.
fn seq(items: &[&XEl], d: usize) -> Result<Seq, String> {
    too_deep(d)?;
    if let [first, inner @ .., last] = items
        && let (Some(o), Some(c)) = (mo_char(first), mo_char(last))
        && OPEN.contains(&o)
        && CLOSE.contains(&c)
        && matching(items)
    {
        let body = seq(inner, d + 1)?;
        return Ok(vec![Node::Delim { open: o.to_string(), close: c.to_string(), sep: String::new(), items: vec![body] }]);
    }
    let mut out: Seq = Vec::new();
    let mut i = 0;
    while let Some(x) = items.get(i) {
        i += 1;
        // `sin ⁡ x`: a named function applied to the next item.
        if x.local() == "mi"
            && token(x).chars().count() > 1
            && items.get(i).and_then(|f| mo_char(f)) == Some('\u{2061}')
            && let Some(arg) = items.get(i + 1)
        {
            let body = el(arg, d + 1)?;
            out.push(Node::Func { name: token(x), body });
            i += 2;
            continue;
        }
        let mut nodes = el(x, d)?;
        if let [Node::Nary { body, .. }] = nodes.as_mut_slice()
            && body.is_empty()
        {
            let start = i;
            while let Some(y) = items.get(i) {
                if mo_char(y).is_some_and(|c| RELATIONS.contains(&c)) {
                    break;
                }
                i += 1;
            }
            *body = seq(items.get(start..i).unwrap_or(&[]), d + 1)?;
        }
        push_all(&mut out, nodes);
    }
    Ok(out)
}

/// True when the first item's fence is closed by the last item (`(a)+(b)` is not one fence).
fn matching(items: &[&XEl]) -> bool {
    let n = items.len();
    if n < 2 {
        return false;
    }
    let mut depth = 0i64;
    for (k, x) in items.iter().enumerate() {
        let Some(c) = mo_char(x) else { continue };
        let (is_open, is_close) = (OPEN.contains(&c), CLOSE.contains(&c));
        if is_open && !(is_close && depth > 0) {
            depth += 1;
        } else if is_close {
            depth -= 1;
            if depth == 0 && k + 1 < n {
                return false;
            }
        }
    }
    depth == 0
}

/// Append, merging adjacent plain text.
fn push_all(out: &mut Seq, nodes: Seq) {
    for n in nodes {
        match (out.last_mut(), n) {
            (Some(Node::Text(a)), Node::Text(b)) => a.push_str(&b),
            (_, n) => out.push(n),
        }
    }
}

/// The `k`-th child element as a row (missing: empty).
fn arg(e: &XEl, k: usize, d: usize) -> Result<Seq, String> {
    match e.els().nth(k) {
        Some(x) => el(x, d + 1),
        None => Ok(Vec::new()),
    }
}

fn opt(s: Seq) -> Option<Seq> {
    if s.is_empty() { None } else { Some(s) }
}

/// A big operator standing alone as a base (`∑` in `munderover`).
fn nary_char(base: &Seq) -> Option<char> {
    match base.as_slice() {
        [Node::Text(t)] => single(t).filter(|c| NARY.contains(c)),
        _ => None,
    }
}

/// Over/under marks that become accents, bars or grouping characters.
fn accent_for(c: char) -> Option<char> {
    Some(match c {
        '^' | 'ˆ' | '\u{302}' => '\u{302}',
        '~' | '˜' | '\u{303}' => '\u{303}',
        '˙' | '.' | '\u{307}' => '\u{307}',
        '¨' | '\u{308}' => '\u{308}',
        '→' | '⃗' => '\u{20D7}',
        'ˇ' | '\u{30C}' => '\u{30C}',
        '´' | '\u{301}' => '\u{301}',
        // `\bar`: the combining overline (equaSHion, OMML) or a macron (MathJax).
        '¯' | '\u{305}' => '\u{305}',
        '`' | '\u{300}' => '\u{300}',
        '˘' | '\u{306}' => '\u{306}',
        _ => return None,
    })
}

fn is_bar(c: char) -> bool {
    matches!(c, '‾' | '_' | '\u{332}' | '−' | '-')
}

fn is_group(c: char) -> bool {
    matches!(c, '⏞' | '⏟' | '︷' | '︸' | '⏜' | '⏝' | '⎴' | '⎵' | '←' | '↔' | '⟶' | '⟵')
}

/// One mark (`mo`/`mi` with a single character) or `None`.
fn mark(s: &Seq) -> Option<char> {
    match s.as_slice() {
        [Node::Text(t)] => single(t),
        [Node::Styled { text, .. }] => single(text),
        _ => None,
    }
}

fn under_over(base: Seq, under: Option<Seq>, over: Option<Seq>) -> Seq {
    if let Some(op) = nary_char(&base) {
        return vec![Node::Nary { op, sub: under, sup: over, body: Vec::new() }];
    }
    let mut cur = base;
    if let Some(u) = under {
        cur = match mark(&u) {
            Some(c) if is_bar(c) => vec![Node::Bar { top: false, body: cur }],
            Some(c) if is_group(c) => vec![Node::GroupChr { ch: c, top: false, body: cur }],
            _ => vec![Node::Limit { lower: true, base: cur, lim: u }],
        };
    }
    if let Some(o) = over {
        cur = match mark(&o) {
            Some(c) if is_bar(c) => vec![Node::Bar { top: true, body: cur }],
            Some(c) if is_group(c) => vec![Node::GroupChr { ch: c, top: true, body: cur }],
            Some(c) if accent_for(c).is_some() => vec![Node::Accent { ch: accent_for(c).unwrap_or(c), body: cur }],
            _ => vec![Node::Limit { lower: false, base: cur, lim: o }],
        };
    }
    cur
}

fn scripts(base: Seq, sub: Option<Seq>, sup: Option<Seq>) -> Seq {
    if let Some(op) = nary_char(&base) {
        return vec![Node::Nary { op, sub, sup, body: Vec::new() }];
    }
    if sub.is_none() && sup.is_none() {
        return base;
    }
    vec![Node::Script { base, sub, sup }]
}

fn variant_style(e: &XEl, single_char: bool) -> Option<Style> {
    match e.attr_local("mathvariant") {
        Some("normal") => Some(Style::Plain),
        Some("bold") => Some(Style::Bold),
        Some("bold-italic") => Some(Style::BoldItalic),
        Some("italic") => None,
        _ if single_char => None,
        _ => Some(Style::Plain),
    }
}

fn styled(text: String, style: Option<Style>) -> Seq {
    if text.is_empty() {
        return Vec::new();
    }
    match style {
        Some(style) => vec![Node::Styled { text, style }],
        None => vec![Node::Text(text)],
    }
}

/// Width of an `mspace` in em (`1em`, `0.2777em`, `thickmathspace`), 0 when unknown.
fn space_em(w: &str) -> f64 {
    let w = w.trim();
    let named = match w {
        "veryverythinmathspace" => Some(1.0 / 18.0),
        "verythinmathspace" => Some(2.0 / 18.0),
        "thinmathspace" => Some(3.0 / 18.0),
        "mediummathspace" => Some(4.0 / 18.0),
        "thickmathspace" => Some(5.0 / 18.0),
        "verythickmathspace" => Some(6.0 / 18.0),
        "veryverythickmathspace" => Some(7.0 / 18.0),
        _ => None,
    };
    if let Some(n) = named {
        return n;
    }
    let (num, scale) = if let Some(n) = w.strip_suffix("em") {
        (n, 1.0)
    } else if let Some(n) = w.strip_suffix("ex") {
        (n, 0.5)
    } else if let Some(n) = w.strip_suffix("pt") {
        (n, 0.1)
    } else if let Some(n) = w.strip_suffix("px") {
        (n, 1.0 / 16.0)
    } else {
        (w, 0.0)
    };
    num.trim().parse::<f64>().ok().filter(|f| f.is_finite()).map_or(0.0, |f| f * scale)
}

/// One element as a row fragment.
fn el(e: &XEl, d: usize) -> Result<Seq, String> {
    too_deep(d)?;
    let local = e.local();
    Ok(match local {
        "mi" => {
            let t = token(e);
            let one = t.chars().count() == 1;
            styled(t, variant_style(e, one))
        }
        "mn" => {
            let style = match e.attr_local("mathvariant") {
                Some("bold") => Some(Style::Bold),
                _ => None,
            };
            styled(token(e), style)
        }
        "mo" => {
            let t: String = token(e).chars().filter(|c| !invisible(*c)).collect();
            styled(t, None)
        }
        "mtext" => styled(token(e), Some(Style::Normal)),
        "ms" => styled(format!("\"{}\"", token(e)), Some(Style::Normal)),
        "mspace" => {
            let em = e.attr_local("width").map_or(0.0, space_em);
            if em >= 0.9 {
                vec![Node::Text("\u{2003}".into())]
            } else if em > 0.0 {
                vec![Node::Text("\u{2009}".into())]
            } else {
                Vec::new()
            }
        }
        "mfrac" => vec![Node::Frac { num: arg(e, 0, d)?, den: arg(e, 1, d)? }],
        "msqrt" => vec![Node::Rad { deg: None, body: row(e, d + 1)? }],
        "mroot" => vec![Node::Rad { deg: opt(arg(e, 1, d)?), body: arg(e, 0, d)? }],
        "msub" => scripts(arg(e, 0, d)?, opt(arg(e, 1, d)?), None),
        "msup" => scripts(arg(e, 0, d)?, None, opt(arg(e, 1, d)?)),
        "msubsup" => scripts(arg(e, 0, d)?, opt(arg(e, 1, d)?), opt(arg(e, 2, d)?)),
        "munder" => under_over(arg(e, 0, d)?, opt(arg(e, 1, d)?), None),
        "mover" => under_over(arg(e, 0, d)?, None, opt(arg(e, 1, d)?)),
        "munderover" => under_over(arg(e, 0, d)?, opt(arg(e, 1, d)?), opt(arg(e, 2, d)?)),
        "mmultiscripts" => multiscripts(e, d)?,
        "mtable" => {
            let mut rows = Vec::new();
            for r in e.els() {
                let cells: Vec<&XEl> = match r.local() {
                    "mtr" => r.els().collect(),
                    // The label cell of a labelled row is not part of the matrix.
                    "mlabeledtr" => r.els().skip(1).collect(),
                    _ => vec![r],
                };
                let mut row_cells = Vec::new();
                for c in cells {
                    row_cells.push(if c.local() == "mtd" { row(c, d + 1)? } else { el(c, d + 1)? });
                }
                rows.push(row_cells);
            }
            if rows.is_empty() { Vec::new() } else { vec![Node::Matrix { rows }] }
        }
        "mfenced" => {
            let open = e.attr_local("open").unwrap_or("(").trim().to_string();
            let close = e.attr_local("close").unwrap_or(")").trim().to_string();
            let sep = e.attr_local("separators").unwrap_or(",").chars().find(|c| !c.is_whitespace()).map(String::from).unwrap_or_default();
            let mut items = Vec::new();
            for x in e.els() {
                items.push(el(x, d + 1)?);
            }
            if items.is_empty() {
                items.push(Vec::new());
            }
            vec![Node::Delim { open, close, sep, items }]
        }
        "menclose" => vec![Node::Boxed { border: true, body: row(e, d + 1)? }],
        "semantics" => match e.els().next() {
            Some(x) => el(x, d + 1)?,
            None => Vec::new(),
        },
        "annotation" | "annotation-xml" | "mphantom" | "none" | "mprescripts" | "maligngroup" | "malignmark" | "mglyph" => Vec::new(),
        // mrow, mstyle, mpadded, merror, math and anything unknown: the children as a row.
        _ => row(e, d + 1)?,
    })
}

fn multiscripts(e: &XEl, d: usize) -> Result<Seq, String> {
    let kids = els(e);
    let base = match kids.first() {
        Some(b) => el(b, d + 1)?,
        None => return Ok(Vec::new()),
    };
    let split = kids.iter().position(|k| k.local() == "mprescripts");
    let post = kids.get(1..split.unwrap_or(kids.len())).unwrap_or(&[]);
    let pre = split.and_then(|s| kids.get(s + 1..)).unwrap_or(&[]);
    let pair = |xs: &[&XEl], k: usize| -> Result<Option<Seq>, String> {
        match xs.get(k) {
            Some(x) => Ok(opt(el(x, d + 1)?)),
            None => Ok(None),
        }
    };
    let (sub, sup) = (pair(post, 0)?, pair(post, 1)?);
    let with_post = scripts(base, sub, sup);
    let (psub, psup) = (pair(pre, 0)?, pair(pre, 1)?);
    if psub.is_none() && psup.is_none() {
        return Ok(with_post);
    }
    Ok(vec![Node::PreScript { sub: psub.unwrap_or_default(), sup: psup.unwrap_or_default(), base: with_post }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{to_linear, to_omml};

    const NS: &str = "http://www.w3.org/1998/Math/MathML";

    fn m(body: &str) -> Result<Math, String> {
        from_mathml(&format!("<math xmlns=\"{NS}\">{body}</math>"))
    }

    #[test]
    fn quadratic_formula() {
        let src = format!(
            "<math xmlns=\"{NS}\" display=\"block\"><mi>x</mi><mo>=</mo><mfrac><mrow><mo>−</mo><mi>b</mi><mo>±</mo><msqrt><msup><mi>b</mi><mn>2</mn></msup><mo>−</mo><mn>4</mn><mi>a</mi><mi>c</mi></msqrt></mrow><mrow><mn>2</mn><mi>a</mi></mrow></mfrac></math>"
        );
        let math = from_mathml(&src).unwrap();
        assert!(math.para);
        assert_eq!(math.body.first(), Some(&Node::Text("x=".into())));
        let Some(Node::Frac { num, den }) = math.body.get(1) else { panic!("{math:?}") };
        assert_eq!(den, &vec![Node::Text("2a".into())]);
        assert!(num.iter().any(|n| matches!(n, Node::Rad { deg: None, .. })));
        assert!(to_omml(&math).contains("<m:f>"));
    }

    #[test]
    fn elements_map_to_the_tree() {
        let r = m("<mroot><mi>x</mi><mn>3</mn></mroot>").unwrap();
        assert!(matches!(r.body.as_slice(), [Node::Rad { deg: Some(_), .. }]));
        let r = m(
            "<msubsup><mo>∑</mo><mrow><mi>i</mi><mo>=</mo><mn>1</mn></mrow><mi>n</mi></msubsup><msup><mi>i</mi><mn>2</mn></msup><mo>=</mo><mi>S</mi>",
        )
        .unwrap();
        let [Node::Nary { op: '∑', sub: Some(_), sup: Some(_), body }, Node::Text(rest)] = r.body.as_slice() else { panic!("{r:?}") };
        assert!(matches!(body.as_slice(), [Node::Script { .. }]));
        assert_eq!(rest, "=S");
        let r = m("<mrow><mo>(</mo><mfrac><mi>a</mi><mi>b</mi></mfrac><mo>)</mo></mrow>").unwrap();
        assert!(matches!(r.body.as_slice(), [Node::Delim { open, close, .. }] if open == "(" && close == ")"));
        // Two separate groups are not one fence.
        let r = m("<mo>(</mo><mi>a</mi><mo>)</mo><mo>+</mo><mo>(</mo><mi>b</mi><mo>)</mo>").unwrap();
        assert!(matches!(r.body.as_slice(), [Node::Text(_)]), "{r:?}");
        let r = m("<mover><mi>x</mi><mo>^</mo></mover><mover><mi>y</mi><mo>‾</mo></mover><munder><mi>lim</mi><mrow><mi>n</mi><mo>→</mo><mi>∞</mi></mrow></munder>").unwrap();
        assert!(
            matches!(r.body.as_slice(), [Node::Accent { ch: '\u{302}', .. }, Node::Bar { top: true, .. }, Node::Limit { lower: true, .. }]),
            "{r:?}"
        );
        let r =
            m("<mtable><mtr><mtd><mn>1</mn></mtd><mtd><mn>0</mn></mtd></mtr><mtr><mtd><mn>0</mn></mtd><mtd><mn>1</mn></mtd></mtr></mtable>").unwrap();
        assert!(matches!(r.body.as_slice(), [Node::Matrix { rows }] if rows.len() == 2 && rows[0].len() == 2));
        let r = m("<mi>sin</mi><mo>\u{2061}</mo><mi>x</mi>").unwrap();
        assert!(matches!(r.body.as_slice(), [Node::Func { name, .. }] if name == "sin"));
        let r = m("<mfenced><mi>a</mi><mi>b</mi></mfenced><mspace width=\"1em\"/><mtext>if</mtext>").unwrap();
        assert!(
            matches!(r.body.as_slice(), [Node::Delim { items, .. }, Node::Text(_), Node::Styled { style: Style::Normal, .. }] if items.len() == 2)
        );
        let r = m("<semantics><mi>y</mi><annotation encoding=\"application/x-tex\">y</annotation></semantics>").unwrap();
        assert_eq!(to_linear(&r).trim(), "y");
        let r = m("<mmultiscripts><mi>C</mi><mprescripts/><mi>n</mi><mi>k</mi></mmultiscripts>").unwrap();
        assert!(matches!(r.body.as_slice(), [Node::PreScript { .. }]));
        // A prefixed root bound to the namespace reads too.
        assert!(from_mathml(&format!("<?xml version=\"1.0\"?><mml:math xmlns:mml=\"{NS}\"><mml:mi>z</mml:mi></mml:math>")).is_ok());
        // No namespace at all (HTML-embedded MathML) is accepted.
        assert!(from_mathml("<math><mi>z</mi></math>").is_ok());
    }

    #[test]
    fn hostile_input_is_an_error_never_a_panic() {
        let deep = format!("<math>{}<mi>x</mi>{}</math>", "<mrow>".repeat(5000), "</mrow>".repeat(5000));
        assert!(from_mathml(&deep).is_err());
        let deep_frac = format!("<math>{}{}</math>", "<mfrac><mi>a</mi>".repeat(80), "</mfrac>".repeat(80));
        assert!(from_mathml(&deep_frac).is_err());
        let wide = format!("<math>{}</math>", "<mi>x</mi>".repeat(300_000));
        assert!(from_mathml(&wide).is_err(), "too large");
        for bad in [
            "",
            "<math>",
            "<math><mi>x</mi>",
            "<math><mi>x</math>",
            "<math></math>",
            "<math><mi> </mi></math>",
            "<math/>",
            "<math><mi>x</mi></math><math><mi>y</mi></math>",
            "<math><mi>x</mi></math> trailing",
            "<math xmlns=\"http://example.com\"><mi>x</mi></math>",
            "<m:math><m:mi>x</m:mi></m:math>",
            "<math><mi a=\"1\" a=\"2\">x</mi></math>",
            "<math><mi a=1>x</mi></math>",
            "<mrow><mi>x</mi></mrow>",
            "<math><mi>x &lt; y</mi><mfrac/><mroot/><msubsup/><munderover/><mtable/><mfenced/><mmultiscripts/></math>",
            "<math><mfrac><mi>a</mi></mfrac><msup/><mtable><mtr/></mtable></math>",
            "<math><mspace width=\"1e400em\"/><mspace width=\"NaNem\"/><mi>x</mi></math>",
            "<math><mo>(</mo><mo>(</mo><mo>)</mo></math>",
            "<math><mo>|</mo><mi>x</mi><mo>|</mo></math>",
            "<math><mo>\u{2061}</mo></math>",
            "<math><mi>sin</mi><mo>\u{2061}</mo></math>",
            "<math><munderover><mo>∫</mo></munderover></math>",
            "<math><![CDATA[x]]></math>",
            "<math><!-- c --><mi>\u{0}</mi></math>",
        ] {
            let _ = from_mathml(bad);
        }
        assert!(from_mathml("<math><mi>x</mi></math><math><mi>y</mi></math>").is_err());
        assert!(from_mathml("<math></math>").is_err());
        assert!(from_mathml("<math xmlns=\"http://example.com\"><mi>x</mi></math>").is_err());
        assert!(from_mathml("<math><mi>x</math>").is_err());
        assert!(from_mathml("<math><mo>|</mo><mi>x</mi><mo>|</mo></math>").is_ok());
    }
}
