//! Equations: an inline object holding the OMML source (canonical, with `xmlns:m`) and its linear
//! text. The commands here create, replace and read those objects; the dialog and agents use
//! them. Every OMML that enters a command is parsed and re-serialised by the math crate first, so
//! only a well-formed `m:oMath` / `m:oMathPara` is ever stored.

use serde_json::{Value, json};
use wordcraft_doc::para::{InlineObject, OBJ};
use wordcraft_doc::{Pos, RevisionKind, StoryRef};
use wordcraft_math::Math;

use super::{delete_selection, new_revision, parse_pos, pos_json};
use crate::{CmdError, CmdResult, CommandSpec, Selection, Session};

/// Largest OMML document accepted from a parameter.
const MAX_OMML: usize = 1 << 20;
/// Largest linear text accepted from a parameter.
const MAX_LINEAR: usize = 1 << 15;
/// Largest `tree` (as JSON text) returned inline; a bigger one is left out (`treeOmitted`).
const MAX_TREE_JSON: usize = 1 << 20;
/// Most input items one `equation.type` call takes.
const MAX_INPUT_ITEMS: usize = 10_000;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("insert.equation", "Equation", "Insert › Symbols", insert_equation)
            .key("Alt+=")
            .params(r#"{"linear"?: string (default "a^2+b^2=c^2") | "omml"?: string, "display"?: bool} → {pos, omml, linear, display}"#),
        CommandSpec::new("equation.update", "Edit Equation", "Equation", update)
            .params(r#"{"linear"? | "omml"?, "display"?: bool, "pos"?: {block, off} | "index"?: n} replaces an equation (default: the one selected or at the caret)"#),
        CommandSpec::new("equation.get", "Get Equation", "Equation", get)
            .params(r#"{"pos"?: {block, off} | "index"?: n} → {pos, index, omml, linear, display, tree}"#)
            .pure(),
        CommandSpec::new("equation.templates", "Equation Templates", "Equation", templates).params("{} → [{id, label, group, linear, omml}]").pure(),
        CommandSpec::new("equation.type", "Type into Equation", "Equation", type_input)
            .params(
                r#"{"input": string | [string], "linear"? | "omml"?, "display"?} → {omml, linear, display, tree, caret: {path, pos}, selection}: types into an equation like the editor does (`x^2`, `a/`, `sqrt`, `alpha`); an item like `<Right>`, `<Shift+Left>`, `<Backspace>` is a key"#,
            )
            .pure(),
        CommandSpec::new("equation.fromLinear", "Linear Text to Equation", "Equation", from_linear)
            .params(r#"{"linear": string, "display"?: bool} → {omml, linear, display, tree}"#)
            .pure(),
    ]
}

fn bad(msg: impl Into<String>) -> CmdError {
    CmdError::Params(msg.into())
}

/// An optional parameter: absent or null is `None`.
fn opt<'a>(p: &'a Value, key: &str) -> Option<&'a Value> {
    p.get(key).filter(|v| !v.is_null())
}

/// An optional boolean: present but of the wrong type is an error (a typo must not fall back).
fn opt_bool(p: &Value, key: &str) -> Result<Option<bool>, CmdError> {
    match opt(p, key) {
        None => Ok(None),
        Some(v) => v.as_bool().map(Some).ok_or_else(|| bad(format!("`{key}` must be true or false"))),
    }
}

/// True when the text's root element is `m:oMath` / `m:oMathPara` (any prefix).
fn root_is_math(t: &str) -> bool {
    let mut rest = t.trim_start();
    // Skip an XML declaration or comments before the root.
    while let Some(r) = rest.strip_prefix("<?").or_else(|| rest.strip_prefix("<!--")) {
        let end = if rest.starts_with("<?") { "?>" } else { "-->" };
        match r.find(end) {
            Some(i) => rest = r.get(i + end.len()..).unwrap_or("").trim_start(),
            None => return false,
        }
    }
    let Some(r) = rest.strip_prefix('<') else { return false };
    let name: String = r.chars().take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/').collect();
    let local = name.rsplit(':').next().unwrap_or("");
    matches!(local, "oMath" | "oMathPara")
}

/// Parse the `omml` / `linear` parameters into a math tree; the OMML reader never fails, so text
/// that is not an equation is rejected here. `display` is applied only when given.
fn math_param(p: &Value, default_linear: Option<&str>) -> Result<Math, CmdError> {
    let display = opt_bool(p, "display")?;
    let mut m = match (opt(p, "omml"), opt(p, "linear")) {
        (Some(x), _) => {
            let x = x.as_str().ok_or_else(|| bad("`omml` must be a string"))?;
            if x.len() > MAX_OMML {
                return Err(bad("`omml` is too large"));
            }
            if !root_is_math(x) {
                return Err(bad("`omml` is not an Office Math (m:oMath) element"));
            }
            wordcraft_math::from_omml(x.trim())
        }
        (None, Some(x)) => {
            let x = x.as_str().ok_or_else(|| bad("`linear` must be a string"))?;
            if x.len() > MAX_LINEAR {
                return Err(bad("`linear` is too large"));
            }
            wordcraft_math::from_linear(x)
        }
        (None, None) => match default_linear {
            Some(d) => wordcraft_math::from_linear(d),
            None => return Err(bad("missing `linear` or `omml`")),
        },
    };
    if m.is_empty() || wordcraft_math::to_linear(&m).trim().is_empty() {
        return Err(bad("the equation is empty"));
    }
    if let Some(d) = display {
        m.para = d;
    }
    Ok(m)
}

/// The object payload for an equation: canonical OMML and the linear text of that OMML (one
/// line, never empty, so the object always has a width).
fn payload(m: &Math) -> (String, String) {
    let omml = wordcraft_math::to_omml(m);
    let linear = wordcraft_math::run_text(&wordcraft_math::from_omml(&omml));
    (omml, linear)
}

fn insert_equation(s: &mut Session, p: &Value) -> CmdResult {
    let m = math_param(p, Some("a^2+b^2=c^2"))?;
    insert_math(s, &m)
}

/// Insert an equation object in place of the selection (tracked when Track Changes is on).
pub(crate) fn insert_math(s: &mut Session, m: &Math) -> CmdResult {
    let (omml, linear) = payload(m);
    let mut props = s.typing_props();
    let at = delete_selection(s)?;
    if s.doc.settings.track_changes {
        props.ins = Some(new_revision(s, RevisionKind::Insert));
        props.del = None;
    } else {
        props.ins = None;
        props.del = None;
    }
    let obj = InlineObject::Equation { linear: linear.clone(), display: m.para, omml: Some(omml.clone()) };
    let end = s.doc.insert_object(&at, obj, &props)?;
    s.sel = Selection::caret(end);
    Ok(json!({"pos": pos_json(&at), "omml": omml, "linear": linear, "display": m.para, "warnings": warnings(m)}))
}

/// Every equation in the body, in document order, as (position of its object character, object).
fn all_equations(s: &Session) -> Vec<Pos> {
    let mut v = Vec::new();
    for path in s.doc.para_paths(StoryRef::Body) {
        let Some(p) = s.doc.para(StoryRef::Body, &path) else { continue };
        for off in p.object_offsets() {
            if matches!(p.object_at(off), Some(InlineObject::Equation { .. })) {
                v.push(Pos { story: StoryRef::Body, path: path.clone(), off });
            }
        }
    }
    v
}

fn is_equation(s: &Session, at: &Pos) -> bool {
    s.doc.para(at.story, &at.path).is_some_and(|p| matches!(p.object_at(at.off), Some(InlineObject::Equation { .. })))
}

/// Where the equation to act on is: `pos` (its object character), `index` (n-th in the body), the
/// one inside the selection or touching the caret, else the first one in the caret's paragraph.
fn locate(s: &Session, p: &Value) -> Result<Pos, CmdError> {
    if let Some(v) = opt(p, "pos") {
        let at = parse_pos(v).ok_or_else(|| bad("`pos` must be {block, off} or a position"))?;
        return if is_equation(s, &at) { Ok(at) } else { Err(bad("there is no equation at `pos`")) };
    }
    if let Some(v) = opt(p, "index") {
        let n = v.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| bad("`index` must be a non-negative integer"))?;
        return all_equations(s).into_iter().nth(n).ok_or_else(|| bad("no such equation"));
    }
    let (a, b) = s.sel.ordered();
    let story = a.story;
    let paths = if a == b { vec![a.path.clone()] } else { s.doc.paths_between(&a, &b) };
    for path in &paths {
        let Some(para) = s.doc.para(story, path) else { continue };
        for off in para.object_offsets() {
            if !matches!(para.object_at(off), Some(InlineObject::Equation { .. })) {
                continue;
            }
            let touching = if a == b {
                *path == a.path && (off == a.off || off + OBJ.len_utf8() == a.off)
            } else {
                (*path != a.path || off >= a.off) && (*path != b.path || off < b.off)
            };
            if touching {
                return Ok(Pos { story, path: path.clone(), off });
            }
        }
    }
    if let Some(para) = s.doc.para(story, &a.path)
        && let Some(off) = para.object_offsets().into_iter().find(|o| matches!(para.object_at(*o), Some(InlineObject::Equation { .. })))
    {
        return Ok(Pos { story, path: a.path.clone(), off });
    }
    Err(bad("no equation found: select one or pass `pos` / `index`"))
}

fn update(s: &mut Session, p: &Value) -> CmdResult {
    let at = locate(s, p)?;
    let mut m = math_param(p, None)?;
    // New linear text keeps the equation's inline / display setting unless `display` says so.
    if opt(p, "omml").is_none()
        && opt_bool(p, "display")?.is_none()
        && let Some(InlineObject::Equation { display, .. }) = s.doc.para(at.story, &at.path).and_then(|q| q.object_at(at.off))
    {
        m.para = *display;
    }
    let (omml, linear) = payload(&m);
    let obj = InlineObject::Equation { linear: linear.clone(), display: m.para, omml: Some(omml.clone()) };
    let mut tracked = false;
    if s.doc.settings.track_changes {
        // The old run's properties say who inserted it (own insertions are replaced in place,
        // as for text) and carry the formatting over to the new equation.
        let author = s.author.clone();
        let old_props =
            s.doc.para(at.story, &at.path).and_then(|q| q.run_ranges().find(|(r, _)| r.start <= at.off && at.off < r.end).map(|(_, c)| c.clone()));
        if let Some(mut props) = old_props {
            let own = props.ins.and_then(|i| s.doc.revisions.get(i as usize)).is_some_and(|rv| rv.author == author);
            if !own {
                let del = new_revision(s, RevisionKind::Delete);
                let ins = new_revision(s, RevisionKind::Insert);
                let end = at.off + OBJ.len_utf8();
                let para = s.doc.para_mut(at.story, &at.path)?;
                para.format(at.off, end, &|c| c.del = Some(del))?;
                props.del = None;
                props.ins = Some(ins);
                para.insert_object(end, obj.clone(), &props)?;
                tracked = true;
            }
        }
    }
    if !tracked {
        let para = s.doc.para_mut(at.story, &at.path)?;
        let slot = para.object_at_mut(at.off).ok_or_else(|| CmdError::Failed("the equation is gone".into()))?;
        *slot = obj;
        para.touch();
    }
    Ok(json!({"pos": pos_json(&at), "omml": omml, "linear": linear, "display": m.para, "tracked": tracked, "warnings": warnings(&m)}))
}

/// Typed commands the parser did not understand; they stay as literal text, so say so.
fn warnings(m: &Math) -> Vec<String> {
    wordcraft_math::unknown_commands(m).into_iter().map(|n| format!("unknown command \\{n}: kept as literal text")).collect()
}

fn describe(m: &Math) -> Value {
    // The tree is far larger than its source; a huge one is withheld so a client never gets a
    // response of hundreds of megabytes.
    let tree = serde_json::to_string(m).ok().filter(|t| t.len() <= MAX_TREE_JSON).and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let omitted = tree.is_none();
    let mut v = json!({
        "omml": wordcraft_math::to_omml(m),
        "linear": wordcraft_math::to_linear(m),
        "display": m.para,
        "tree": tree.unwrap_or(Value::Null),
        "warnings": warnings(m),
    });
    if let (true, Some(o)) = (omitted, v.as_object_mut()) {
        o.insert("treeOmitted".into(), json!(true));
    }
    v
}

fn get(s: &mut Session, p: &Value) -> CmdResult {
    let at = locate(s, p)?;
    let Some(InlineObject::Equation { linear, display, omml }) = s.doc.para(at.story, &at.path).and_then(|q| q.object_at(at.off)).cloned() else {
        return Err(CmdError::Failed("the equation is gone".into()));
    };
    let mut m = match &omml {
        Some(x) => wordcraft_math::from_omml(x),
        None => wordcraft_math::from_linear(&linear),
    };
    m.para = m.para || display;
    let mut v = describe(&m);
    let index = all_equations(s).iter().position(|q| *q == at);
    if let Some(o) = v.as_object_mut() {
        o.insert("pos".into(), pos_json(&at));
        o.insert("index".into(), json!(index));
    }
    Ok(v)
}

fn templates(_s: &mut Session, _p: &Value) -> CmdResult {
    let list: Vec<Value> = wordcraft_math::templates()
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "label": t.label,
                "group": t.group,
                "linear": wordcraft_math::to_linear(&t.math),
                "omml": wordcraft_math::to_omml(&t.math),
            })
        })
        .collect();
    Ok(Value::Array(list))
}

fn from_linear(_s: &mut Session, p: &Value) -> CmdResult {
    let text = opt(p, "linear").and_then(Value::as_str).ok_or_else(|| bad("`linear` must be a string"))?;
    if text.len() > MAX_LINEAR {
        return Err(bad("`linear` is too large"));
    }
    let mut m = wordcraft_math::from_linear(text);
    if let Some(d) = opt_bool(p, "display")? {
        m.para = d;
    }
    Ok(describe(&m))
}

/// Type into an equation as the editor does: characters build structure, `<Key>` items press keys.
fn type_input(_s: &mut Session, p: &Value) -> CmdResult {
    let items: Vec<&str> = match opt(p, "input") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(a)) if a.len() <= MAX_INPUT_ITEMS => {
            a.iter().map(|x| x.as_str().ok_or_else(|| bad("`input` items must be strings"))).collect::<Result<_, _>>()?
        }
        _ => return Err(bad("`input` must be a string or a list of at most 10000 strings")),
    };
    if items.iter().map(|t| t.len()).sum::<usize>() > MAX_LINEAR {
        return Err(bad("`input` is too large"));
    }
    let mut m = if opt(p, "omml").is_some() || opt(p, "linear").is_some() { math_param(p, None)? } else { Math::default() };
    if let Some(d) = opt_bool(p, "display")? {
        m.para = d;
    }
    let mut ed = wordcraft_math::Editor::new(m);
    for item in items {
        match item.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
            Some(name) => {
                let (shift, key) = match name.split_once('+') {
                    Some((m, k)) if m.eq_ignore_ascii_case("shift") => (true, k),
                    _ => (false, name),
                };
                let key = wordcraft_math::Key::parse(key).ok_or_else(|| bad(format!("unknown key `{item}`")))?;
                ed.key(key, shift);
            }
            None => ed.type_str(item),
        }
    }
    let mut v = describe(ed.math());
    let c = ed.caret();
    if let Some(o) = v.as_object_mut() {
        o.insert("caret".into(), json!({"path": c.path.iter().map(|(i, k)| json!([i, k])).collect::<Vec<_>>(), "pos": c.pos}));
        o.insert("selection".into(), ed.selection().map_or(Value::Null, |(a, b)| json!([a, b])));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use wordcraft_doc::StoryRef;
    use wordcraft_doc::para::InlineObject;

    use crate::Session;

    const NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/math";

    fn session() -> Session {
        Session::new(wordcraft_doc::Document::new())
    }

    fn eq_at(s: &Session, block: usize) -> (String, bool, Option<String>) {
        let p = s.doc.para(StoryRef::Body, &wordcraft_doc::Path(vec![block as u32])).unwrap();
        for off in p.object_offsets() {
            if let Some(InlineObject::Equation { linear, display, omml }) = p.object_at(off) {
                return (linear.clone(), *display, omml.clone());
            }
        }
        panic!("no equation in block {block}");
    }

    #[test]
    fn insert_stores_canonical_omml_and_undoes() {
        let mut s = session();
        let r = s.run("insert.equation", &json!({"linear": "x/2"})).unwrap();
        assert!(r["omml"].as_str().unwrap().contains("xmlns:m="));
        let (lin, display, omml) = eq_at(&s, 0);
        assert!(!display && !lin.is_empty());
        assert!(omml.unwrap().contains("<m:f>"));
        assert_eq!(s.sel.focus.off, "\u{FFFC}".len(), "caret after the equation");
        s.run("edit.undo", &json!({})).unwrap();
        assert!(s.doc.para(StoryRef::Body, &wordcraft_doc::Path(vec![0])).unwrap().objects.is_empty());
        s.run("edit.redo", &json!({})).unwrap();
        assert_eq!(eq_at(&s, 0).0, lin);
    }

    #[test]
    fn latex_big_operators_scope_their_whole_operand_and_keep_spacing() {
        let mut s = session();
        let r = s.run("equation.fromLinear", &json!({"linear": "\\sum_{i=1}^n (i+1)^2"})).unwrap();
        let omml = r["omml"].as_str().unwrap();
        // One n-ary whose body holds the squared parenthesis: the exponent is not on the sum.
        assert_eq!(omml.matches("<m:nary>").count() + omml.matches("<m:nary ").count(), 1, "{omml}");
        assert!(omml.contains("</m:sup><m:e><m:sSup>"), "{omml}");
        assert!(r["warnings"].as_array().unwrap().is_empty());
        let r = s.run("equation.fromLinear", &json!({"linear": "\\int_a^b f(x)\\,dx"})).unwrap();
        assert!(r["omml"].as_str().unwrap().contains('\u{2009}'), "thin space kept");
        let r = s.run("equation.fromLinear", &json!({"linear": "\\overset{def}{=} \\lVert x \\rVert \\mathbf{v}"})).unwrap();
        assert!(r["warnings"].as_array().unwrap().is_empty(), "{r}");
    }

    #[test]
    fn unknown_commands_are_reported_and_cfrac_is_a_fraction() {
        let mut s = session();
        let r = s.run("equation.fromLinear", &json!({"linear": "1+\\cfrac{1}{x}+\\bogus y"})).unwrap();
        assert!(r["omml"].as_str().unwrap().contains("<m:f>"), "cfrac parses as a fraction");
        let w = r["warnings"].as_array().unwrap();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].as_str().unwrap().contains("bogus"));
        let r = s.run("insert.equation", &json!({"linear": "\\alpha+\\nope"})).unwrap();
        assert_eq!(r["warnings"].as_array().unwrap().len(), 1);
        let r = s.run("equation.fromLinear", &json!({"linear": "x^2"})).unwrap();
        assert!(r["warnings"].as_array().unwrap().is_empty());
    }

    #[test]
    fn brace_labels_are_limits_in_the_stored_omml() {
        let mut s = session();
        let r = s.run("equation.fromLinear", &json!({"linear": "\\underbrace{a+b}_{n} \\overrightarrow{AB} f\\prime"})).unwrap();
        let o = r["omml"].as_str().unwrap();
        assert!(o.contains("<m:limLow>") && o.contains("<m:groupChr>"), "{o}");
        assert!(!o.contains("<m:sSub>"), "{o}");
        assert!(r["warnings"].as_array().unwrap().is_empty(), "{r}");
    }

    #[test]
    fn default_equation_is_pythagoras() {
        let mut s = session();
        s.run("insert.equation", &json!({})).unwrap();
        assert!(eq_at(&s, 0).0.contains('c'));
    }

    #[test]
    fn update_get_round_trip_and_undo() {
        let mut s = session();
        s.run("insert.equation", &json!({"linear": "x/2", "display": true})).unwrap();
        let before = eq_at(&s, 0);
        // Selection touches the equation (caret right after it): no pos needed.
        let r = s.run("equation.update", &json!({"linear": "sqrt(x)+1"})).unwrap();
        assert!(r["omml"].as_str().unwrap().contains("m:rad"));
        let after = eq_at(&s, 0);
        assert!(after.1, "display kept when only linear is given");
        assert_ne!(after.0, before.0);
        let g = s.run("equation.get", &json!({"index": 0})).unwrap();
        assert_eq!(g["display"], true);
        assert_eq!(g["index"], 0);
        assert!(g["tree"].is_object());
        s.run("equation.update", &json!({"linear": "z", "display": false})).unwrap();
        assert!(!eq_at(&s, 0).1);
        s.run("edit.undo", &json!({})).unwrap();
        assert_eq!(eq_at(&s, 0), after);
        s.run("edit.undo", &json!({})).unwrap();
        assert_eq!(eq_at(&s, 0), before);
        s.run("edit.redo", &json!({})).unwrap();
        assert_eq!(eq_at(&s, 0), after);
    }

    #[test]
    fn update_accepts_omml_and_it_round_trips_through_the_math_crate() {
        let mut s = session();
        s.run("insert.equation", &json!({"linear": "1"})).unwrap();
        let omml =
            format!(r#"<m:oMath xmlns:m="{NS}"><m:f><m:num><m:r><m:t>a</m:t></m:r></m:num><m:den><m:r><m:t>b</m:t></m:r></m:den></m:f></m:oMath>"#);
        s.run("equation.update", &json!({"omml": omml})).unwrap();
        let (_, display, stored) = eq_at(&s, 0);
        let stored = stored.unwrap();
        assert!(!display);
        assert!(stored.contains("<m:f>") && stored.contains("xmlns:m="));
        assert_eq!(wordcraft_math::to_omml(&wordcraft_math::from_omml(&stored)), stored, "stored OMML is canonical");
        // Garbage that merely parses as XML never reaches the model.
        let evil = format!(r#"<m:oMath xmlns:m="{NS}"><x:evil xmlns:x="urn:q" onclick="1"/><m:r><m:t>q</m:t></m:r></m:oMath>"#);
        s.run("equation.update", &json!({"omml": evil})).unwrap();
        let stored = eq_at(&s, 0).2.unwrap();
        assert_eq!(wordcraft_math::to_omml(&wordcraft_math::from_omml(&stored)), stored);
    }

    #[test]
    fn display_update_via_omml_para_sets_flag() {
        let mut s = session();
        s.run("insert.equation", &json!({"linear": "x"})).unwrap();
        let omml = format!(r#"<m:oMathPara xmlns:m="{NS}"><m:oMath><m:r><m:t>q</m:t></m:r></m:oMath></m:oMathPara>"#);
        s.run("equation.update", &json!({"omml": omml})).unwrap();
        let (_, display, stored) = eq_at(&s, 0);
        assert!(display && stored.unwrap().contains("oMathPara"));
    }

    #[test]
    fn bad_input_is_an_error_and_changes_nothing() {
        let mut s = session();
        s.run("insert.equation", &json!({"linear": "x/2"})).unwrap();
        let before = eq_at(&s, 0);
        let hist = s.undo_labels().len();
        for p in [
            json!({"linear": ""}),
            json!({"linear": "   "}),
            json!({"omml": "not xml at all"}),
            json!({"omml": "<<<"}),
            json!({"omml": "<a><b/></a>"}),
            json!({"omml": "<a>oMath</a>"}),
            json!({"omml": "<a><m:oMath/></a>"}),
            json!({"omml": format!("<m:oMath xmlns:m=\"{NS}\"></m:oMath>")}),
            json!({"omml": 5}),
            json!({"linear": 5}),
            json!({"linear": "y", "display": "yes"}),
            json!({"linear": "y", "pos": {"block": 0, "off": 1}}),
            json!({"linear": "y", "pos": 7}),
            json!({"linear": "y", "pos": {"block": 99, "off": 0}}),
            json!({"linear": "y", "index": -1}),
            json!({"linear": "y", "index": 1.5}),
            json!({"linear": "y", "index": 99}),
            json!({"linear": "x".repeat(super::MAX_LINEAR + 1)}),
            json!({"omml": format!("<m:oMath xmlns:m=\"{NS}\">{}</m:oMath>", "x".repeat(super::MAX_OMML))}),
        ] {
            assert!(s.run("equation.update", &p).is_err(), "update {p:.80}");
            if p.get("index").is_none() && p.get("pos").is_none() {
                assert!(s.run("insert.equation", &p).is_err(), "insert {p:.80}");
            }
        }
        assert_eq!(eq_at(&s, 0), before);
        assert_eq!(s.undo_labels().len(), hist, "failed commands leave no undo step");
        assert!(s.run("equation.get", &json!({"pos": {"block": 0, "off": 0}})).is_ok());
    }

    #[test]
    fn no_equation_is_an_error() {
        let mut s = session();
        s.run("text.insert", &json!({"text": "plain"})).unwrap();
        assert!(s.run("equation.get", &json!({})).is_err());
        assert!(s.run("equation.update", &json!({"linear": "x"})).is_err());
    }

    #[test]
    fn typed_text_and_equations_share_a_paragraph() {
        let mut s = session();
        s.run("text.insert", &json!({"text": "so "})).unwrap();
        s.run("insert.equation", &json!({"linear": "a^2"})).unwrap();
        s.run("text.insert", &json!({"text": " done"})).unwrap();
        let g = s.run("equation.get", &json!({})).unwrap();
        assert_eq!(g["pos"]["off"], 3);
        s.run("equation.update", &json!({"linear": "b^3", "pos": g["pos"]})).unwrap();
        assert!(s.doc.plain_text(StoryRef::Body).starts_with("so "));
        assert_eq!(s.doc.plain_text(StoryRef::Body).matches(" done").count(), 1);
    }

    #[test]
    fn tracked_update_replaces_and_reject_restores() {
        let mut s = session();
        s.run("insert.equation", &json!({"linear": "y^2"})).unwrap();
        let before = eq_at(&s, 0);
        s.doc.settings.track_changes = true;
        let r = s.run("equation.update", &json!({"linear": "y^3", "index": 0})).unwrap();
        assert_eq!(r["tracked"], true);
        assert!(s.doc.revisions.iter().any(|r| r.kind == wordcraft_doc::RevisionKind::Delete));
        s.run("review.rejectAll", &json!({})).unwrap();
        assert_eq!(eq_at(&s, 0), before, "reject restores the old equation");
        s.run("edit.undo", &json!({})).unwrap();
        s.run("review.acceptAll", &json!({})).unwrap();
        assert!(eq_at(&s, 0).0.contains('3'));
        assert_eq!(s.doc.para(StoryRef::Body, &wordcraft_doc::Path(vec![0])).unwrap().objects.len(), 1);
        // Editing your own tracked insertion replaces it in place.
        s.run("insert.equation", &json!({"linear": "q"})).unwrap();
        let n = s.doc.revisions.len();
        let r = s.run("equation.update", &json!({"linear": "w", "index": 1})).unwrap();
        assert_eq!(r["tracked"], false);
        assert_eq!(s.doc.revisions.len(), n);
    }

    #[test]
    fn tracked_insert_marks_the_equation() {
        let mut s = session();
        s.doc.settings.track_changes = true;
        s.run("insert.equation", &json!({"linear": "x"})).unwrap();
        assert!(!s.doc.revisions.is_empty());
    }

    #[test]
    fn templates_and_from_linear_are_queries() {
        let mut s = session();
        let t = s.run("equation.templates", &json!({})).unwrap();
        let list = t.as_array().unwrap();
        assert!(list.len() > 10);
        assert!(list.iter().any(|x| x["id"] == "frac"));
        assert!(list.iter().all(|x| x["omml"].as_str().unwrap().contains("xmlns:m=")));
        let r = s.run("equation.fromLinear", &json!({"linear": "(a+b)/c", "display": true})).unwrap();
        assert!(r["omml"].as_str().unwrap().contains("<m:f>"));
        assert_eq!(r["display"], true);
        assert!(s.run("equation.fromLinear", &json!({})).is_err());
        assert!(s.run("equation.fromLinear", &json!({"linear": "y", "display": 3})).is_err());
        assert!(!s.can_undo(), "queries add no undo step");
    }

    #[test]
    fn type_builds_structure_like_the_editor() {
        let mut s = session();
        let r = s.run("equation.type", &json!({"input": "x^2"})).unwrap();
        assert_eq!(r["linear"].as_str().unwrap().replace(' ', ""), "x^2");
        assert_eq!(r["caret"]["path"], json!([[0, 1]]));
        assert!(r["omml"].as_str().unwrap().contains("m:sSup"));
        let r = s.run("equation.type", &json!({"input": ["a/", "b", "<Right>", "+", "sqrt", "x", "<Right>", "alpha"]})).unwrap();
        let l = r["linear"].as_str().unwrap().replace(' ', "");
        assert!(l.contains('√') && l.contains('α') && l.contains('/'), "{l}");
        // Typed LaTeX is understood like the linear text is.
        let r = s.run("equation.type", &json!({"input": ["\\frac{a}{b}", "+", "\\alpha", "\\lim_{x\\to0}"]})).unwrap();
        let l = r["linear"].as_str().unwrap().replace(' ', "");
        assert!(l.starts_with("a/b+α") && !l.contains("\\") && !l.contains("()_"), "{l}");
        let r = s.run("equation.type", &json!({"linear": "ab", "input": ["<Shift+Left>"]})).unwrap();
        assert_eq!(r["selection"], json!([1, 2]));
        assert!(s.doc.para(StoryRef::Body, &wordcraft_doc::Path(vec![0])).unwrap().objects.is_empty(), "equation.type is a query");
    }

    #[test]
    fn type_rejects_bad_input_and_survives_hostile_keys() {
        let mut s = session();
        for p in [
            json!({}),
            json!({"input": 5}),
            json!({"input": [1]}),
            json!({"input": ["<Sideways>"]}),
            json!({"input": "x", "display": "yes"}),
            json!({"input": "x", "omml": "junk"}),
        ] {
            assert!(s.run("equation.type", &p).is_err(), "{p}");
        }
        assert!(s.run("equation.type", &json!({"input": "x".repeat(super::MAX_LINEAR + 1)})).is_err());
        let many: Vec<&str> = (0..5000).map(|i| ["^", "_", "/", "sqrt", "<Left>", "<Backspace>", "<Up>", "<Right>", "<Down>"][i % 9]).collect();
        assert!(s.run("equation.type", &json!({"input": many})).is_ok());
    }

    #[test]
    fn huge_results_are_bounded() {
        let mut s = session();
        let big = "a_".repeat(super::MAX_LINEAR / 2);
        let r = s.run("equation.fromLinear", &json!({"linear": big})).unwrap();
        assert!(serde_json::to_string(&r).unwrap().len() < 8 << 20);
    }

    #[test]
    fn control_characters_never_reach_the_linear_text() {
        let mut s = session();
        s.run("insert.equation", &json!({"linear": "a\nb\r\nc\td\u{0}e"})).unwrap();
        let (lin, _, _) = eq_at(&s, 0);
        assert!(!lin.chars().any(char::is_control), "{lin:?}");
    }

    #[test]
    fn hostile_equation_params_never_panic() {
        let junk: Vec<Value> = vec![
            json!(null),
            json!([]),
            json!({"pos": null, "index": null}),
            json!({"pos": {"story": "body", "path": [9999999], "off": 99999999}, "linear": "x"}),
            json!({"pos": {"block": 0, "off": 1e308}, "linear": "x"}),
            json!({"index": 1e308, "linear": "x"}),
            json!({"input": vec!["^"; 20000]}),
            json!({"linear": "(".repeat(100_000)}),
            json!({"linear": "x^".repeat(10_000)}),
            json!({"omml": format!("<m:oMath xmlns:m=\"{NS}\">{}{}</m:oMath>", "<m:f><m:num>".repeat(5000), "</m:num></m:f>".repeat(5000))}),
        ];
        for id in ["insert.equation", "equation.update", "equation.get", "equation.templates", "equation.type", "equation.fromLinear"] {
            for j in &junk {
                for tracked in [false, true] {
                    let mut s = session();
                    s.run("insert.equation", &json!({"linear": "x/2"})).unwrap();
                    s.doc.settings.track_changes = tracked;
                    let _ = s.run(id, j);
                    s.clamp_selection();
                    let _ = s.layout();
                }
            }
        }
    }

    /// The exact text the equaSHion tool put on the clipboard for the quadratic formula.
    const QUADRATIC_OMML: &str = "<m:oMath xmlns:m=\"http://schemas.openxmlformats.org/officeDocument/2006/math\"><m:r><m:t xml:space=\"preserve\">x=</m:t></m:r><m:f><m:num><m:r><m:t xml:space=\"preserve\">−b±</m:t></m:r><m:rad><m:radPr><m:degHide m:val=\"1\"/></m:radPr><m:deg></m:deg><m:e><m:sSup><m:e><m:r><m:t xml:space=\"preserve\">b</m:t></m:r></m:e><m:sup><m:r><m:t xml:space=\"preserve\">2</m:t></m:r></m:sup></m:sSup><m:r><m:t xml:space=\"preserve\">−4ac</m:t></m:r></m:e></m:rad></m:num><m:den><m:r><m:t xml:space=\"preserve\">2a</m:t></m:r></m:den></m:f></m:oMath>";

    const QUADRATIC_MATHML: &str = "<math xmlns=\"http://www.w3.org/1998/Math/MathML\" display=\"block\"><mi>x</mi><mo>=</mo><mfrac><mrow><mo>−</mo><mi>b</mi><mo>±</mo><msqrt><msup><mi>b</mi><mn>2</mn></msup><mo>−</mo><mn>4</mn><mi>a</mi><mi>c</mi></msqrt></mrow><mrow><mn>2</mn><mi>a</mi></mrow></mfrac></math>";

    fn body_text(s: &Session) -> String {
        s.doc.para_paths(StoryRef::Body).iter().filter_map(|p| s.doc.para(StoryRef::Body, p)).map(|p| p.text.clone()).collect::<Vec<_>>().join("\n")
    }

    fn equation_count(s: &Session) -> usize {
        s.doc
            .para_paths(StoryRef::Body)
            .iter()
            .filter_map(|p| s.doc.para(StoryRef::Body, p))
            .map(|p| p.objects.iter().filter(|o| matches!(o, InlineObject::Equation { .. })).count())
            .sum()
    }

    /// The first equation's tree as the `equation.get` command reports it.
    fn tree(s: &mut Session) -> Value {
        s.run("equation.get", &json!({"index": 0})).unwrap()
    }

    fn has_node(v: &Value, kind: &str) -> bool {
        match v {
            Value::Object(o) => o.contains_key(kind) || o.values().any(|x| has_node(x, kind)),
            Value::Array(a) => a.iter().any(|x| has_node(x, kind)),
            _ => false,
        }
    }

    #[test]
    fn pasting_the_equashion_omml_inserts_an_equation_in_one_undo_step() {
        let mut s = session();
        s.run("text.insert", &json!({"text": "Roots: "})).unwrap();
        let before = body_text(&s);
        let r = s.run("edit.paste", &json!({"text": QUADRATIC_OMML})).unwrap();
        assert_eq!(r["pasted"], "equation");
        assert_eq!(r["format"], "OMML");
        assert_eq!(equation_count(&s), 1);
        assert!(!body_text(&s).contains("oMath"), "no markup pasted as text");
        let g = tree(&mut s);
        assert!(has_node(&g["tree"], "Frac") && has_node(&g["tree"], "Rad") && has_node(&g["tree"], "Script"), "{g}");
        assert_eq!(g["display"], false);
        assert_eq!(g["linear"].as_str().unwrap().replace(' ', ""), "x=(−b±√(b^2−4ac))/2a".replace(' ', ""), "{g}");
        s.run("edit.undo", &json!({})).unwrap();
        assert_eq!(equation_count(&s), 0);
        assert_eq!(body_text(&s), before, "one undo removes the pasted equation and nothing else");
        s.run("edit.redo", &json!({})).unwrap();
        assert_eq!(equation_count(&s), 1);
    }

    #[test]
    fn pasting_omml_with_header_and_crlf_and_para_root() {
        let mut s = session();
        let text = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n<m:oMathPara xmlns:m=\"{NS}\">{}</m:oMathPara>\r\n",
            QUADRATIC_OMML.replacen(&format!(" xmlns:m=\"{NS}\""), "", 1)
        );
        let r = s.run("edit.paste", &json!({"text": text})).unwrap();
        assert_eq!(r["pasted"], "equation");
        assert_eq!(tree(&mut s)["display"], true);
    }

    #[test]
    fn pasting_mathml_and_delimited_latex_inserts_equations() {
        let mut s = session();
        let r = s.run("edit.paste", &json!({"text": QUADRATIC_MATHML})).unwrap();
        assert_eq!(r["format"], "MathML");
        let g = tree(&mut s);
        assert!(has_node(&g["tree"], "Frac") && has_node(&g["tree"], "Rad"), "{g}");
        assert_eq!(g["display"], true);
        s.run("edit.undo", &json!({})).unwrap();
        assert_eq!(equation_count(&s), 0);
        // The HTML half of equaSHion's Copy MathML, should a paste deliver it as the text.
        let html = format!("<html><body><!--StartFragment-->{QUADRATIC_MATHML}<!--EndFragment--></body></html>");
        let r = s.run("edit.paste", &json!({"text": html})).unwrap();
        assert_eq!(r["format"], "MathML");
        assert_eq!(equation_count(&s), 1);
        s.run("edit.undo", &json!({})).unwrap();
        for (text, display) in [
            ("$x=\\frac{-b\\pm\\sqrt{b^2-4ac}}{2a}$", false),
            ("\\(x=\\frac{-b\\pm\\sqrt{b^2-4ac}}{2a}\\)", false),
            ("$$x=\\frac{-b\\pm\\sqrt{b^2-4ac}}{2a}$$", true),
            ("\\[x=\\frac{-b\\pm\\sqrt{b^2-4ac}}{2a}\\]", true),
        ] {
            let mut s = session();
            let r = s.run("edit.paste", &json!({"text": text})).unwrap();
            assert_eq!(r["format"], "LaTeX", "{text}");
            let g = tree(&mut s);
            assert!(has_node(&g["tree"], "Frac") && has_node(&g["tree"], "Rad"), "{text}: {g}");
            assert_eq!(g["display"], display, "{text}");
            assert_eq!(body_text(&s).chars().count(), 1, "only the equation object: {text}");
        }
    }

    #[test]
    fn plain_text_keep_text_only_and_malformed_markup_stay_text() {
        for text in ["hello world", "costs $5 and $6", "a \\frac{1}{2} b", "\\alpha", "<b>bold</b>"] {
            let mut s = session();
            s.run("edit.paste", &json!({"text": text})).unwrap();
            assert_eq!(equation_count(&s), 0, "{text}");
            assert_eq!(body_text(&s), text);
        }
        // Keep Text Only pastes the markup itself.
        for text in [QUADRATIC_OMML, QUADRATIC_MATHML, "$x^2$"] {
            let mut s = session();
            s.run("edit.pasteText", &json!({"text": text})).unwrap();
            assert_eq!(equation_count(&s), 0);
            assert_eq!(body_text(&s), text);
        }
        // Broken OMML / MathML: the clipboard is pasted as text, with a note.
        let truncated = QUADRATIC_OMML.trim_end_matches("</m:oMath>");
        for text in [truncated, "<m:oMath/>", "<m:oMath xmlns:m=\"urn:x\"><m:r><m:t>a</m:t></m:r></m:oMath>", "<math><mi>x</mi>", "<math></math>"] {
            let mut s = session();
            s.status.clear();
            let r = s.run("edit.paste", &json!({"text": text})).unwrap();
            assert!(r.get("pasted").is_none(), "{text}");
            assert_eq!(equation_count(&s), 0, "{text}");
            assert_eq!(body_text(&s), text);
            assert!(s.status.starts_with("Pasted as text"), "{}", s.status);
            s.run("edit.undo", &json!({})).unwrap();
            assert_eq!(body_text(&s), "");
        }
    }

    #[test]
    fn pasted_equation_is_a_tracked_insertion_with_track_changes_on() {
        let mut s = session();
        s.doc.settings.track_changes = true;
        s.run("edit.paste", &json!({"text": QUADRATIC_OMML})).unwrap();
        let p = s.doc.para(StoryRef::Body, &wordcraft_doc::Path(vec![0])).unwrap();
        let (_, props) = p.run_ranges().find(|(r, _)| p.text.get(r.clone()).is_some_and(|t| t.contains('\u{FFFC}'))).unwrap();
        assert!(props.ins.is_some(), "tracked as an insertion");
    }

    #[test]
    fn hostile_mathml_and_omml_pastes_never_panic_and_keep_the_text() {
        let deep = format!("<math>{}<mi>x</mi>{}</math>", "<mrow>".repeat(5000), "</mrow>".repeat(5000));
        let deep_frac = format!("<math>{}<mi>x</mi>{}</math>", "<mfrac><mi>a</mi>".repeat(200), "</mfrac>".repeat(200));
        let wide = format!("<math>{}</math>", "<mi>x</mi>".repeat(200_000));
        let deep_omml = format!("<m:oMath xmlns:m=\"{NS}\">{}{}</m:oMath>", "<m:f><m:num>".repeat(5000), "</m:num></m:f>".repeat(5000));
        let cases = [
            deep,
            deep_frac,
            wide,
            deep_omml,
            "<math><mi a=\"1\" a=\"2\">x</mi></math>".into(),
            "<math><mi>x</mi></math><math><mi>y</mi></math>".into(),
            "<math xmlns=\"urn:evil\"><mi>x</mi></math>".into(),
            "<math><mfrac/><mroot/><msubsup/><munderover/><mtable><mtr/></mtable><mfenced/><mmultiscripts/></math>".into(),
            "<math><mo>(</mo><mo>(</mo><mo>)</mo></math>".into(),
            "<math><mspace width=\"1e400em\"/></math>".into(),
            "$\\frac{".into(),
            format!("${}$", "{".repeat(20_000)),
        ];
        for text in &cases {
            for tracked in [false, true] {
                let mut s = session();
                s.doc.settings.track_changes = tracked;
                let r = s.run("edit.paste", &json!({"text": text}));
                assert!(r.is_ok(), "{:.60}: {r:?}", text);
                if equation_count(&s) == 0 {
                    assert_eq!(body_text(&s), *text, "the clipboard is never lost");
                }
                let _ = s.layout();
            }
        }
    }
}
