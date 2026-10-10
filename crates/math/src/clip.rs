//! Recognise an equation in pasted plain text: the whole text (trimmed) is exactly one OMML
//! `m:oMath` / `m:oMathPara`, one MathML `<math>`, or LaTeX in explicit delimiters (`$$..$$`,
//! `\[..\]` display; `$..$`, `\(..\)` inline). Anything else is ordinary text.

use crate::{Math, from_linear, from_mathml, from_omml, to_linear, xml};

/// Largest OMML or MathML paste read as an equation.
pub const MAX_MARKUP: usize = 1 << 20;
/// Largest LaTeX paste read as an equation.
pub const MAX_LATEX: usize = 1 << 15;

/// What kind of equation payload a paste looked like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipKind {
    Omml,
    MathMl,
    Latex,
}

impl ClipKind {
    pub fn name(self) -> &'static str {
        match self {
            ClipKind::Omml => "OMML",
            ClipKind::MathMl => "MathML",
            ClipKind::Latex => "LaTeX",
        }
    }
}

/// `None` when the text is not an equation payload (paste it as text). Otherwise its kind and
/// the parsed equation, or why it could not be read (the caller pastes the text and says so).
pub fn equation_from_clipboard(text: &str) -> Option<(ClipKind, Result<Math, String>)> {
    let t = text.trim();
    // An HTML clipboard fragment whose only content is the equation (as browsers and the
    // equaSHion tool write it) reads like the bare element.
    let t = html_fragment(t).filter(|f| matches!(root_local(f), Some((_, "oMath" | "oMathPara" | "math")))).unwrap_or(t);
    match root_local(t) {
        Some((_, "oMath" | "oMathPara")) => return Some((ClipKind::Omml, omml(t))),
        Some((_, "math")) => return Some((ClipKind::MathMl, from_mathml(t))),
        _ => {}
    }
    let (inner, display) = latex_inner(t)?;
    Some((ClipKind::Latex, latex(inner, display)))
}

/// The content of an HTML clipboard fragment: between `<!--StartFragment-->` and
/// `<!--EndFragment-->`, else inside `<body>`, trimmed. `None` when the text is not HTML.
fn html_fragment(t: &str) -> Option<&str> {
    if let (Some(a), Some(b)) = (t.find("<!--StartFragment-->"), t.rfind("<!--EndFragment-->")) {
        return t.get(a + "<!--StartFragment-->".len()..b).map(str::trim);
    }
    let lower = t.chars().take(16).collect::<String>().to_ascii_lowercase();
    if !(lower.starts_with("<html") || lower.starts_with("<!doctype html") || lower.starts_with("<body")) {
        return None;
    }
    let open = t.find("<body")?;
    let start = open + t.get(open..)?.find('>')? + 1;
    let end = t.rfind("</body>")?;
    t.get(start..end).map(str::trim)
}

/// Prefix and local name of the first element, after an XML declaration, processing
/// instructions and comments.
fn root_local(t: &str) -> Option<(&str, &str)> {
    let mut rest = t;
    while let Some(r) = rest.strip_prefix("<?").or_else(|| rest.strip_prefix("<!--")) {
        let end = if rest.starts_with("<?") { "?>" } else { "-->" };
        let i = r.find(end)?;
        rest = r.get(i + end.len()..)?.trim_start();
    }
    let r = rest.strip_prefix('<')?;
    let len = r.find(|c: char| c.is_whitespace() || c == '>' || c == '/').unwrap_or(r.len());
    let name = r.get(..len)?;
    Some(match name.split_once(':') {
        Some((p, l)) => (p, l),
        None => ("", name),
    })
}

fn omml(t: &str) -> Result<Math, String> {
    if t.len() > MAX_MARKUP {
        return Err("the OMML is too large".into());
    }
    xml::well_formed(t).map_err(|e| format!("not well-formed OMML: {e}"))?;
    let roots = xml::parse(t);
    match roots.as_slice() {
        [r] if r.math => {}
        _ => return Err("the root element is not in the Office Math namespace".into()),
    }
    let m = from_omml(t);
    if m.is_empty() || to_linear(&m).trim().is_empty() {
        return Err("the OMML equation is empty".into());
    }
    Ok(m)
}

/// The LaTeX between explicit delimiters spanning the whole text, and whether it is display.
fn latex_inner(t: &str) -> Option<(&str, bool)> {
    let pick = |open: &str, close: &str, display: bool| -> Option<(&str, bool)> {
        let inner = t.strip_prefix(open)?.strip_suffix(close)?;
        // The delimiters must enclose the whole text: no other delimiter inside.
        if inner.contains(open) || inner.contains(close) || inner.trim().is_empty() {
            return None;
        }
        Some((inner, display))
    };
    if t.starts_with("$$") {
        return pick("$$", "$$", true).filter(|(i, _)| !i.contains('$'));
    }
    pick("\\[", "\\]", true).or_else(|| pick("\\(", "\\)", false)).or_else(|| pick("$", "$", false))
}

fn latex(inner: &str, display: bool) -> Result<Math, String> {
    if inner.len() > MAX_LATEX {
        return Err("the LaTeX is too long".into());
    }
    let mut m = from_linear(inner.trim());
    if m.is_empty() || to_linear(&m).trim().is_empty() {
        return Err("the LaTeX equation is empty".into());
    }
    m.para = display;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Node;

    #[test]
    fn detects_only_whole_payloads() {
        for t in ["", "hello", "costs $5", "$5 and $6", "a \\frac{1}{2} b", "\\alpha", "$$", "$ $", "$a$ and $b$", "<b>x</b>", "<mathx>y</mathx>"] {
            assert!(equation_from_clipboard(t).is_none(), "{t:?}");
        }
        let (k, r) = equation_from_clipboard("  $x^2$\n").unwrap();
        assert_eq!(k, ClipKind::Latex);
        assert!(!r.unwrap().para);
        let (_, r) = equation_from_clipboard("$$\\frac{a}{b}$$").unwrap();
        let m = r.unwrap();
        assert!(m.para && matches!(m.body.as_slice(), [Node::Frac { .. }]));
        assert!(equation_from_clipboard("\\[x\\]").unwrap().1.unwrap().para);
        assert!(!equation_from_clipboard("\\(x\\)").unwrap().1.unwrap().para);
        let (k, r) = equation_from_clipboard("<m:oMath><m:r><m:t>x</m:t></m:r>").unwrap();
        assert_eq!(k, ClipKind::Omml);
        assert!(r.is_err(), "unclosed OMML is not an equation");
        assert!(equation_from_clipboard("<m:oMath/>").unwrap().1.is_err(), "empty");
        assert!(equation_from_clipboard("<x:oMath xmlns:x=\"urn:other\"><x:r><x:t>a</x:t></x:r></x:oMath>").unwrap().1.is_err());
        let (k, r) = equation_from_clipboard("<?xml version=\"1.0\"?>\n<m:oMathPara xmlns:m=\"http://schemas.openxmlformats.org/officeDocument/2006/math\"><m:oMath><m:r><m:t>y</m:t></m:r></m:oMath></m:oMathPara>").unwrap();
        assert_eq!(k, ClipKind::Omml);
        assert!(r.unwrap().para);
        let (k, r) = equation_from_clipboard("<math><mi>x</mi></math>").unwrap();
        assert_eq!(k, ClipKind::MathMl);
        assert!(r.is_ok());
        let long = format!("${}$", "x".repeat(MAX_LATEX + 1));
        assert!(equation_from_clipboard(&long).unwrap().1.is_err());
        assert!(equation_from_clipboard("<!-- unterminated").is_none());
        assert!(equation_from_clipboard("<?xml").is_none());
        // HTML clipboard fragments holding only the equation.
        let html = "<html><body><!--StartFragment--><math xmlns=\"http://www.w3.org/1998/Math/MathML\" display=\"block\"><mi>x</mi></math><!--EndFragment--></body></html>";
        let (k, r) = equation_from_clipboard(html).unwrap();
        assert_eq!(k, ClipKind::MathMl);
        assert!(r.unwrap().para);
        let (k, r) = equation_from_clipboard("<html><body>\n<m:oMath><m:r><m:t>y</m:t></m:r></m:oMath>\n</body></html>").unwrap();
        assert_eq!(k, ClipKind::Omml);
        assert!(r.is_ok());
        // Other HTML, or an equation with other content around it, stays text.
        assert!(equation_from_clipboard("<html><body><!--StartFragment--><p>hi</p><!--EndFragment--></body></html>").is_none());
        assert!(equation_from_clipboard("<html><body><!--StartFragment-->see <math><mi>x</mi></math><!--EndFragment--></body></html>").is_none());
        assert!(equation_from_clipboard("<!--EndFragment--><!--StartFragment-->").is_none());
        assert!(equation_from_clipboard("<html><body>").is_none());
    }
}
