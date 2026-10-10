//! The Equation dialog: type the formatted equation straight into the rendered canvas.
//!
//! The canvas shows the equation as it will be inserted (laid out by the real text layout) with a
//! blinking caret inside it. Typing `x`, `^`, `2` raises the 2 at once and leaves the caret in the
//! exponent; `/` makes a fraction, `sqrt` a root, Greek names become symbols, the arrow keys walk
//! the tree and a click places the caret. The editing itself lives in `wordcraft_math::Editor`
//! (pure, tested); this file draws it and turns egui input into editor calls. A template palette
//! and a linear-text field are secondary ways in.
//!
//! The equation is edited in place: every change is applied to the document at once through
//! `insert.equation` (the first one) and `equation.update` (the rest, joined into one undo step),
//! so the page behind the dialog shows it at document size, selected. Insert / Update / Enter
//! keep it; Cancel, Esc twice and the close button undo it. With Track Changes on the document is
//! left alone until Insert / Update. Re-editing loads `equation.get`.

use std::sync::Arc;

use wordcraft_doc::Rgb;

use egui::{Color32, Key, Sense, TextureHandle, TextureOptions, Ui, pos2, vec2};
use serde_json::{Value, json};
use wordcraft_engine::Session;
use wordcraft_math::{Caret, Editor, Key as MKey, Math, from_linear, from_omml, templates, to_linear, to_omml};

use crate::WordApp;
use crate::theme::{self, Tokens};

/// Size of the canvas, in points.
const CANVAS_W: f32 = 540.0;
const CANVAS_H: f32 = 100.0;
/// Font size of the equation on the canvas, points.
const EQ_PT: f64 = 28.0;
/// Space around the equation inside the canvas layout, points.
const EQ_PAD: f64 = 8.0;
/// Family the canvas uses (the default theme font).
const EQ_FAMILY: &str = "Cambria Math";
/// Render scale of the canvas texture (pixels per point).
const CANVAS_SCALE: f64 = 2.0;
/// Largest side of the canvas texture, pixels (egui's guaranteed minimum is 2048).
const MAX_TEXTURE: f64 = 2000.0;
/// Gap between the canvas edge and the equation, points.
const VIEW_MARGIN: f32 = 10.0;
/// Smallest scale a long equation is shrunk to before the canvas scrolls instead.
const MIN_SCALE: f32 = 0.5;
/// Seconds per half blink of the caret.
const BLINK: f64 = 0.5;

/// Palette symbols: Greek letters and operators.
pub const GREEK_LOWER: &str = "αβγδεζηθικλμνξοπρστυφχψω";
pub const GREEK_UPPER: &str = "ΓΔΘΛΞΠΣΦΨΩ";
pub const OPERATORS: &str = "±∓×÷·∘∗≠≈≡≅∼∝≤≥≪≫≺≻∞∂∇∅∈∉⊂⊃⊆⊇∪∩∧∨¬∀∃→←↔⇒⇔↦∴∵…⋯°′″";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Structures,
    Greek,
    Operators,
}

/// The equation as drawn: the tree it was drawn from, its layout and the texture.
struct Canvas {
    shown: Math,
    layout: wordcraft_layout::math::EqLayout,
    tex: TextureHandle,
}

/// Everything the open dialog keeps between frames.
#[derive(Clone)]
pub struct EqState {
    ed: Editor,
    /// Text of the linear field.
    linear: String,
    /// Frames left in which the canvas asks for the keyboard (the dialog's first frames are a
    /// sizing pass that cannot take focus).
    auto_focus: u8,
    /// The canvas is where typing goes: it takes the keyboard back whenever nothing else (the
    /// linear field) holds it, so a click on a palette button does not leave typing dead.
    wants_canvas: bool,
    tab: Tab,
    /// Where an edited equation lives (`id`, `cell`, `paragraph`, `run`); `None` inserts a new one.
    target: Option<Value>,
    error: Option<String>,
    canvas: Option<Arc<Canvas>>,
    /// Time of the last edit, so the caret stays solid while typing.
    touched: f64,
    /// True while the mouse button that started in the canvas is held.
    dragging: bool,
    /// Where the mouse press (or the shift-click's fixed end) is: selections run from here.
    origin: Caret,
    /// Enter was pressed: insert or update.
    submit: bool,
    /// The equation the dialog opened with, to tell whether Escape would lose work.
    initial: Math,
    /// Escape was pressed once with unsaved work: the next one discards it.
    discard_armed: bool,
    /// Where the canvas was drawn last frame: top-left of the equation and points per layout point.
    view: (egui::Pos2, f32),
    /// The dialog was opened on an equation that was already in the document.
    existing: bool,
    /// The document holds this dialog's edit (one undo step) that Cancel must take back.
    live: bool,
    /// Insert / Update was pressed and succeeded: the edit stays.
    committed: bool,
    /// The tree changed since the document last got it.
    unsynced: bool,
    /// The OMML the document holds now, to skip identical updates.
    last_sent: Option<String>,
}

impl std::fmt::Debug for EqState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EqState").field("linear", &self.linear).field("caret", self.ed.caret()).finish_non_exhaustive()
    }
}

impl EqState {
    fn blank() -> Self {
        EqState {
            ed: Editor::new(Math::default()),
            linear: String::new(),
            auto_focus: 4,
            wants_canvas: true,
            tab: Tab::Structures,
            target: None,
            error: None,
            canvas: None,
            touched: 0.0,
            dragging: false,
            origin: Caret::default(),
            submit: false,
            initial: Math::default(),
            discard_armed: false,
            view: (pos2(0.0, 0.0), 1.0),
            existing: false,
            live: false,
            committed: false,
            unsynced: false,
            last_sent: None,
        }
    }

    /// Open on the equation at `pos` (`equation.get`), or blank when `pos` is `None`.
    pub fn load(session: &mut Session, pos: Option<Value>) -> Self {
        let mut st = Self::blank();
        let Some(pos) = pos else { return st };
        let loc = json!({"pos": pos});
        match session.run("equation.get", &loc) {
            Ok(v) => {
                if let Some(o) = v.get("omml").and_then(Value::as_str) {
                    st.ed = Editor::new(from_omml(o));
                    st.linear = to_linear(st.ed.math());
                    st.initial = st.ed.math().clone();
                    // Update the very equation that was read, even if the selection moves meanwhile.
                    st.target = Some(json!({"pos": v.get("pos").cloned().unwrap_or(pos)}));
                    st.existing = true;
                    st.last_sent = Some(o.to_string());
                }
            }
            Err(e) => st.error = Some(e.to_string()),
        }
        st
    }

    /// True when the dialog edits an existing equation (Update) rather than inserting one.
    pub fn is_edit(&self) -> bool {
        self.existing
    }

    /// The text of the linear field changed: rebuild the tree from it.
    fn linear_edited(&mut self) {
        let para = self.ed.math().para;
        let mut m = from_linear(&self.linear);
        m.para = para;
        self.ed.set_math(m);
        self.unsynced = true;
    }

    /// The tree changed through the canvas or the palette: show it in the linear field.
    fn tree_edited(&mut self) {
        self.linear = to_linear(self.ed.math());
        self.error = None;
        self.discard_armed = false;
        self.unsynced = true;
    }

    /// True when closing now would lose an edit.
    fn dirty(&self) -> bool {
        self.ed.math() != &self.initial
    }
}

/// Escape in the Equation dialog: the first press with unsaved work only warns, the second
/// closes. True when the dialog may close.
fn escape_closes(st: &mut EqState) -> bool {
    if !st.dirty() || st.discard_armed {
        return true;
    }
    st.discard_armed = true;
    st.error = Some(tl!("Press Esc again to discard this equation, or Insert to keep it.").into());
    false
}

/// Egui input to editor calls. True when the tree or the caret changed.
fn handle_input(ui: &Ui, st: &mut EqState) -> bool {
    let events = ui.input(|i| i.events.clone());
    if events.is_empty() {
        return false;
    }
    let before = (st.ed.math().clone(), st.ed.caret().clone(), st.ed.selection());
    for ev in &events {
        match ev {
            egui::Event::Text(t) => st.ed.type_str(t),
            egui::Event::Paste(t) => st.ed.type_str(t),
            egui::Event::Copy => {
                if let Some(t) = st.ed.selected_linear() {
                    ui.ctx().copy_text(t);
                }
            }
            egui::Event::Cut => {
                if let Some(t) = st.ed.cut() {
                    ui.ctx().copy_text(t);
                }
            }
            egui::Event::Key { key, pressed: true, modifiers, .. } => {
                let nav = match key {
                    Key::ArrowLeft => Some(MKey::Left),
                    Key::ArrowRight => Some(MKey::Right),
                    Key::ArrowUp => Some(MKey::Up),
                    Key::ArrowDown => Some(MKey::Down),
                    Key::Home => Some(MKey::Home),
                    Key::End => Some(MKey::End),
                    Key::Backspace => Some(MKey::Backspace),
                    Key::Delete => Some(MKey::Delete),
                    Key::Tab => Some(MKey::Tab),
                    Key::Enter => {
                        st.submit = true;
                        None
                    }
                    Key::Z if modifiers.command => Some(if modifiers.shift { MKey::Redo } else { MKey::Undo }),
                    Key::Y if modifiers.command => Some(MKey::Redo),
                    _ => None,
                };
                if let Some(k) = nav {
                    st.ed.key(k, modifiers.shift);
                } else if *key == Key::A && modifiers.command {
                    st.ed.select_all();
                }
            }
            _ => {}
        }
    }
    let tree = before.0 != *st.ed.math();
    if tree {
        st.tree_edited();
    }
    tree || before.1 != *st.ed.caret() || before.2 != st.ed.selection()
}

/// Lay the current tree out and render it, unless the last canvas already shows it.
fn refresh_canvas(ui: &Ui, st: &mut EqState) {
    let shown = st.ed.display_math();
    if st.canvas.as_ref().is_some_and(|c| c.shown == shown) {
        return;
    }
    let layout = wordcraft_layout::math::layout_equation(&shown, EQ_FAMILY, EQ_PT, true, Rgb(22, 22, 30), EQ_PAD);
    // Never beyond the largest texture the GPU takes, however wide the equation grows.
    let scale = (MAX_TEXTURE / layout.width.max(layout.height).max(1.0)).min(CANVAS_SCALE);
    let img = wordcraft_render::render_eq(&layout, scale);
    let ci = egui::ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.to_straight());
    let tex = ui.ctx().load_texture("equation-canvas", ci, TextureOptions::LINEAR);
    st.canvas = Some(Arc::new(Canvas { shown, layout, tex }));
}

fn linear_id() -> egui::Id {
    egui::Id::new("eq-linear")
}

fn canvas_id() -> egui::Id {
    egui::Id::new("eq-canvas")
}

/// The editing canvas: the rendered equation, selection, caret, placeholders and mouse.
fn canvas(ui: &mut Ui, st: &mut EqState) {
    let t = Tokens::get(ui.ctx());
    let id = canvas_id();
    if st.auto_focus > 0 {
        st.auto_focus -= 1;
        ui.memory_mut(|m| m.request_focus(id));
    }
    if ui.memory(|m| m.has_focus(linear_id())) {
        st.wants_canvas = false;
    } else if st.wants_canvas {
        ui.memory_mut(|m| m.request_focus(id));
    }
    // Keys the editor uses must not move the focus away.
    let filter = egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: false };
    ui.memory_mut(|m| m.set_focus_lock_filter(id, filter));

    let (rect, _) = ui.allocate_exact_size(vec2(CANVAS_W, CANVAS_H), Sense::hover());
    let resp = ui.interact(rect, id, Sense::click_and_drag()).on_hover_cursor(egui::CursorIcon::Text);
    let focused = resp.has_focus();
    let now = ui.input(|i| i.time);

    if focused && handle_input(ui, st) {
        st.touched = now;
    }
    refresh_canvas(ui, st);
    let Some(cv) = st.canvas.clone() else { return };

    // Where the equation sits: left-aligned so it grows to the right as you type, vertically
    // centred, shrunk (to at most half size) to fit, then scrolled to keep the caret in view.
    let (w, h) = (cv.layout.width as f32, cv.layout.height as f32);
    let room = CANVAS_W - 2.0 * VIEW_MARGIN;
    let k = (room / w.max(1.0)).max(MIN_SCALE).min(CANVAS_H / h.max(1.0)).min(1.0);
    let size = vec2(w * k, h * k);
    let caret_x = cv.layout.caret_rect(st.ed.caret()).map_or(0.0, |r| r.x1 as f32 * k);
    let scroll = if size.x > room { (caret_x - room * 0.85).clamp(0.0, size.x - room) } else { 0.0 };
    let origin = pos2(rect.min.x + VIEW_MARGIN - scroll, rect.center().y - size.y / 2.0);
    st.view = (origin, k);
    let to_layout = |p: egui::Pos2| ((p.x - origin.x) / k.max(1e-3), (p.y - origin.y) / k.max(1e-3));

    // Mouse: press places the caret, dragging or shift-click selects (across rows too).
    let (pressed, down, pointer) = ui.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_down(), i.pointer.interact_pos()));
    if let Some(p) = pointer {
        let (x, y) = to_layout(p);
        if pressed && rect.contains(p) {
            ui.memory_mut(|m| m.request_focus(id));
            st.wants_canvas = true;
            let hit = cv.layout.hit(f64::from(x), f64::from(y));
            if ui.input(|i| i.modifiers.shift) {
                // Shift-click: extend from the fixed end of the current selection.
                st.origin = st.ed.anchor_caret();
                st.ed.select_between(&st.origin, &hit);
            } else {
                st.ed.set_caret(hit.clone());
                st.origin = hit;
            }
            st.dragging = true;
            st.touched = now;
        } else if st.dragging && down {
            let hit = cv.layout.hit(f64::from(x), f64::from(y));
            st.ed.select_between(&st.origin, &hit);
        }
    }
    if !down {
        st.dragging = false;
    }

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, Color32::WHITE);
    let to_screen = |r: kurbo::Rect| {
        egui::Rect::from_min_max(
            pos2(origin.x + r.x0 as f32 * k, origin.y + r.y0 as f32 * k),
            pos2(origin.x + r.x1 as f32 * k, origin.y + r.y1 as f32 * k),
        )
    };
    // The active place, tinted like MathLive's active placeholder: empty slots become a
    // visible box, filled ones a soft band behind their content. The top level stays plain.
    if !st.ed.caret().path.is_empty()
        && let Some(r) = cv.layout.row_rect(&st.ed.caret().path)
    {
        let r = to_screen(r).expand2(vec2(2.5, 1.0));
        painter.rect_filled(r, 2.5, t.accent.gamma_multiply(0.13));
        painter.rect_stroke(r, 2.5, egui::Stroke::new(1.0, t.accent.gamma_multiply(0.45)), egui::StrokeKind::Inside);
    }
    // Selection under the glyphs.
    if let Some((a, b)) = st.ed.selection()
        && let Some(r) = cv.layout.span_rect(&st.ed.caret().path, a, b)
    {
        painter.rect_filled(to_screen(r), 1.0, t.accent.gamma_multiply(0.28));
    }
    painter.image(cv.tex.id(), egui::Rect::from_min_size(origin, size), egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    if st.ed.is_blank() {
        painter.text(
            pos2(rect.min.x + 14.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            tl!("Type here: x^2, a/b, \\frac, \\sqrt, \\alpha"),
            theme::regular(13.0),
            Color32::from_gray(150),
        );
    }
    // The caret: solid while editing, blinking when idle.
    if focused {
        let on = now - st.touched < BLINK || (((now - st.touched) / BLINK) as u64).is_multiple_of(2);
        if on && let Some(r) = cv.layout.caret_rect(st.ed.caret()) {
            let mut r = to_screen(r);
            // Keep the caret off a radical's bar or a bracket at the start of a place.
            if st.ed.caret().pos == 0 && !st.ed.caret().path.is_empty() {
                r = r.translate(vec2(2.0 * k.max(0.5), 0.0));
            }
            painter.line_segment([pos2(r.min.x, r.min.y), pos2(r.min.x, r.max.y)], egui::Stroke::new(1.6, t.accent));
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
    if !focused {
        painter.text(
            pos2(rect.max.x - 8.0, rect.min.y + 6.0),
            egui::Align2::RIGHT_TOP,
            tl!("Click here to type"),
            theme::regular(11.0),
            Color32::from_gray(140),
        );
    }
    let stroke = if focused { egui::Stroke::new(2.0, t.accent) } else { egui::Stroke::new(1.0, t.border) };
    painter.rect_stroke(rect, 3.0, stroke, egui::StrokeKind::Inside);
}

/// The short face of a structure's palette button.
fn glyph(id: &str) -> Option<&'static str> {
    Some(match id {
        "frac" => "a/b",
        "sup" => "x^n",
        "sub" => "x_n",
        "subsup" => "x_n^m",
        "sqrt" => "\u{221A}",
        "root" => "n\u{221A}",
        "sum" => "\u{2211}",
        "prod" => "\u{220F}",
        "int" => "\u{222B}",
        "intlim" => "\u{222B}ab",
        "paren" => "( )",
        "bracket" => "[ ]",
        "brace" => "{ }",
        "abs" => "| |",
        "matrix2" => "2\u{00D7}2",
        "matrix3" => "3\u{00D7}3",
        "sin" | "cos" | "tan" | "log" | "ln" | "lim" => return None,
        "cases" => "{ cases",
        "overline" => "over",
        "underline" => "under",
        "overbrace" => "brace",
        _ => return None,
    })
}

fn palette(ui: &mut Ui, st: &mut EqState) {
    ui.horizontal(|ui| {
        for (tab, name) in [(Tab::Structures, tl!("Structures")), (Tab::Greek, tl!("Greek")), (Tab::Operators, tl!("Operators"))] {
            if ui.selectable_label(st.tab == tab, name).clicked() {
                st.tab = tab;
            }
        }
    });
    let mut chosen_symbol: Option<char> = None;
    let mut chosen_template: Option<Math> = None;
    match st.tab {
        Tab::Structures => {
            // Every group at once, compact buttons in one flow; the full name is the tooltip.
            let all = templates();
            for g in ["Fractions", "Scripts", "Radicals", "Large operators", "Brackets", "Matrices", "Functions", "Accents"] {
                ui.horizontal_wrapped(|ui| {
                    ui.add_sized(
                        vec2(92.0, 24.0),
                        egui::Label::new(egui::RichText::new(g).font(theme::semibold(11.0)).color(Tokens::get(ui.ctx()).text_dim)),
                    );
                    for tpl in all.iter().filter(|t| t.group == g) {
                        let text = egui::RichText::new(glyph(tpl.id).unwrap_or(tpl.label)).size(15.0);
                        let b = ui.add(egui::Button::new(text).min_size(vec2(30.0, 24.0))).on_hover_text(format!(
                            "{}  ({})",
                            tpl.label,
                            to_linear(&tpl.math)
                        ));
                        if b.clicked() {
                            chosen_template = Some(tpl.math.clone());
                        }
                    }
                });
            }
        }
        Tab::Greek | Tab::Operators => {
            let sets: Vec<&str> = if st.tab == Tab::Greek { vec![GREEK_LOWER, GREEK_UPPER] } else { vec![OPERATORS] };
            for chars in sets {
                ui.horizontal_wrapped(|ui| {
                    for c in chars.chars() {
                        if ui.add(egui::Button::new(egui::RichText::new(c.to_string()).size(16.0)).min_size(vec2(26.0, 26.0))).clicked() {
                            chosen_symbol = Some(c);
                        }
                    }
                });
            }
        }
    }
    if let Some(tpl) = chosen_template {
        st.ed.insert_template(&tpl);
        st.wants_canvas = true;
        st.tree_edited();
        st.touched = ui.input(|i| i.time);
    }
    if let Some(sym) = chosen_symbol {
        st.ed.insert_symbol(sym);
        st.wants_canvas = true;
        st.tree_edited();
        st.touched = ui.input(|i| i.time);
    }
}

/// Apply the edited tree to the document so the page shows it (no-op while Track Changes is on,
/// for a blank tree, or when the document already has exactly this). True when the document
/// holds the tree afterwards.
fn sync_live(app: &mut WordApp, st: &mut EqState, force: bool) -> bool {
    if !force && !st.unsynced {
        return false;
    }
    if app.session.doc.settings.track_changes || st.ed.is_blank() {
        return false;
    }
    st.unsynced = false;
    let omml = to_omml(st.ed.math());
    if st.last_sent.as_deref() == Some(omml.as_str()) {
        return true;
    }
    let mut p = json!({"omml": omml});
    let update = st.target.is_some();
    if let (true, Some(t), Some(o)) = (update, &st.target, p.as_object_mut())
        && let Some(src) = t.as_object()
    {
        o.extend(src.clone());
    }
    if update && st.live {
        // One undo step for the whole editing session.
        app.session.join_next_undo();
    }
    match app.run(if update { "equation.update" } else { "insert.equation" }, p) {
        Ok(v) => {
            st.live = true;
            st.last_sent = Some(omml);
            if st.target.is_none()
                && let Some(pos) = v.get("pos")
            {
                st.target = Some(json!({"pos": pos}));
            }
            // Select the equation on the page: it reads as the thing being edited.
            let at = st.target.as_ref().and_then(|t| t.get("pos")).and_then(|p| serde_json::from_value::<wordcraft_doc::Pos>(p.clone()).ok());
            if let Some(at) = at {
                let end = wordcraft_doc::Pos { off: at.off + wordcraft_doc::para::OBJ.len_utf8(), ..at.clone() };
                app.session.sel = wordcraft_engine::Selection { anchor: at, focus: end };
            }
            true
        }
        Err(e) => {
            st.error = Some(e);
            false
        }
    }
}

/// The dialog is going away: take back the live edit unless it was kept, and leave the caret
/// after the equation when it was.
pub fn finish(app: &mut WordApp, st: &mut EqState) {
    if st.committed {
        if let Some(at) = st.target.as_ref().and_then(|t| t.get("pos")).and_then(|p| serde_json::from_value::<wordcraft_doc::Pos>(p.clone()).ok()) {
            let end = wordcraft_doc::Pos { off: at.off + wordcraft_doc::para::OBJ.len_utf8(), ..at };
            app.session.sel = wordcraft_engine::Selection::caret(end);
        }
    } else if st.live && matches!(app.session.undo_label(), Some("Equation" | "Edit Equation")) {
        let _ = app.run("edit.undo", json!({}));
    }
    st.live = false;
}

/// Draw the dialog body; true when the dialog should close.
pub fn body(app: &mut WordApp, ui: &mut Ui, st: &mut EqState) -> bool {
    ui.set_min_width(CANVAS_W + 14.0);
    palette(ui, st);
    ui.add_space(4.0);
    canvas(ui, st);
    ui.add_space(4.0);

    // Linear text: the secondary way in.
    ui.horizontal(|ui| {
        ui.add_sized(vec2(110.0, 20.0), egui::Label::new(tl!("Linear text:")));
        let r = ui.add(
            egui::TextEdit::singleline(&mut st.linear)
                .id(linear_id())
                .desired_width(400.0)
                .font(theme::regular(14.0))
                .hint_text("a^2+b^2=c^2, x=(-b±sqrt(b^2-4ac))/2a"),
        );
        if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            st.submit = true;
        }
        if r.has_focus() || r.gained_focus() {
            st.wants_canvas = false;
        }
        if r.changed() {
            st.linear_edited();
        }
    });
    let mut display = st.ed.math().para;
    if ui.checkbox(&mut display, tl!("Display on its own line")).changed() {
        st.ed.math_mut().para = display;
        st.unsynced = true;
    }
    sync_live(app, st, false);
    // One line is always reserved for the error, so showing it does not move the buttons.
    ui.allocate_ui_with_layout(vec2(CANVAS_W, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        if let Some(e) = &st.error {
            ui.colored_label(Tokens::get(ui.ctx()).red, e);
        }
    });

    let esc = ui.input(|i| i.key_pressed(Key::Escape));
    let (mut ok, mut cancel) = (std::mem::take(&mut st.submit), false);
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(tl!("Cancel")).clicked() {
                cancel = true;
            }
            let label = if st.is_edit() { tl!("Update") } else { tl!("Insert") };
            let b = ui.add(egui::Button::new(egui::RichText::new(label).color(egui::Color32::WHITE)).fill(theme::APP_COLOR));
            ok |= b.clicked();
        });
    });
    if esc && escape_closes(st) {
        cancel = true;
    }
    if ok {
        st.ed.flush();
        if st.ed.is_blank() {
            st.error = Some(tl!("Type or build an equation first.").to_string());
            return false;
        }
        st.unsynced = true;
        if sync_live(app, st, true) {
            st.committed = true;
            return true;
        }
        if st.error.is_some() {
            return false;
        }
        // Track Changes: the document was not touched while editing; apply it now.
        let update = st.target.is_some();
        let mut p = json!({"omml": to_omml(st.ed.math())});
        if let (Some(t), Some(o)) = (&st.target, p.as_object_mut())
            && let Some(src) = t.as_object()
        {
            o.extend(src.clone());
        }
        return match app.run(if update { "equation.update" } else { "insert.equation" }, p) {
            Ok(_) => {
                st.committed = true;
                true
            }
            Err(e) => {
                st.error = Some(e);
                false
            }
        };
    }
    cancel
}

#[cfg(test)]
mod tests {
    use egui::Modifiers;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use wordcraft_doc::para::InlineObject;
    use wordcraft_math::Caret;

    use super::*;
    use crate::Services;
    use crate::dialogs::Dialog;

    fn app() -> WordApp {
        WordApp::new(Session::new(wordcraft_doc::Document::new()), Services::default())
    }

    fn harness(app: WordApp) -> Harness<'static, WordApp> {
        harness_sized(app, vec2(900.0, 1000.0))
    }

    fn harness_sized(app: WordApp, size: egui::Vec2) -> Harness<'static, WordApp> {
        let mut fonts = false;
        Harness::builder().with_size(size).build_ui_state(
            move |ui, app: &mut WordApp| {
                // The first frame only installs the fonts; they apply from the next one.
                if !fonts {
                    fonts = true;
                    theme::install_fonts(ui.ctx());
                } else {
                    crate::dialogs::show(app, ui.ctx());
                }
            },
            app,
        )
    }

    fn open(app: &mut WordApp) {
        app.dialog = Dialog::open("equation", app);
    }

    fn opened() -> Harness<'static, WordApp> {
        let mut a = app();
        open(&mut a);
        let mut h = harness(a);
        h.run();
        h.run();
        h
    }

    fn eq_count(app: &WordApp) -> usize {
        let doc = &app.session.doc;
        doc.para_paths(wordcraft_doc::StoryRef::Body)
            .iter()
            .filter_map(|p| doc.para(wordcraft_doc::StoryRef::Body, p))
            .map(|p| p.object_offsets().into_iter().filter(|o| matches!(p.object_at(*o), Some(InlineObject::Equation { .. }))).count())
            .sum()
    }

    fn st<'a>(h: &'a Harness<'static, WordApp>) -> &'a EqState {
        match h.state().dialog.as_ref() {
            Some(Dialog::Equation { st }) => st,
            _ => panic!("the equation dialog is not open"),
        }
    }

    fn st_mut<'a>(h: &'a mut Harness<'static, WordApp>) -> &'a mut EqState {
        match h.state_mut().dialog.as_mut() {
            Some(Dialog::Equation { st }) => st,
            _ => panic!("the equation dialog is not open"),
        }
    }

    fn typ(h: &mut Harness<'static, WordApp>, text: &str) {
        h.event(egui::Event::Text(text.into()));
        h.run();
    }

    fn press(h: &mut Harness<'static, WordApp>, key: Key) {
        h.key_press(key);
        h.run();
    }

    fn lin(h: &Harness<'static, WordApp>) -> String {
        to_linear(st(h).ed.math()).replace(' ', "").replace(['(', ')'], "")
    }

    #[test]
    fn short_window_keeps_palette_and_buttons_on_screen() {
        let mut a = app();
        open(&mut a);
        let mut h = harness_sized(a, vec2(900.0, 640.0));
        h.run();
        h.run();
        h.get_by_label("\u{221A}").click_accesskit();
        h.run();
        for label in ["a/b", "Insert", "Cancel"] {
            let r = h.get_by_label(label).rect();
            assert!(r.min.y >= 0.0 && r.max.y <= 640.0, "{label} off screen: {r:?}");
        }
    }

    #[test]
    fn typing_x_caret_2_raises_the_exponent_at_once() {
        let mut h = opened();
        typ(&mut h, "x");
        typ(&mut h, "^");
        assert_eq!(st(&h).ed.caret().path.len(), 1);
        typ(&mut h, "2");
        assert_eq!(lin(&h), "x^2");
        let s = st(&h);
        assert!(matches!(s.ed.math().body.first(), Some(wordcraft_math::Node::Script { sup: Some(_), .. })));
        let cv = s.canvas.as_ref().expect("canvas");
        let sup = cv.layout.caret_rect(s.ed.caret()).expect("sup caret");
        let base = cv.layout.caret_rect(&Caret { path: vec![], pos: 0 }).expect("base caret");
        assert!(sup.y0 < base.y0, "{sup:?} {base:?}");
        assert!(cv.tex.size()[0] > 1);
        press(&mut h, Key::ArrowRight);
        assert!(st(&h).ed.caret().path.is_empty());
        typ(&mut h, "+1");
        assert_eq!(lin(&h), "x^2+1");
        assert_eq!(st(&h).linear.replace(' ', ""), "x^2+1", "the linear field follows");
    }

    #[test]
    fn underscore_slash_sqrt_and_greek_names() {
        let mut h = opened();
        typ(&mut h, "a_1");
        assert_eq!(lin(&h), "a_1");
        press(&mut h, Key::ArrowRight);
        typ(&mut h, "/");
        assert!(matches!(st(&h).ed.math().body.first(), Some(wordcraft_math::Node::Frac { .. })));
        assert_eq!(st(&h).ed.caret().path, vec![(0, 1)]);
        typ(&mut h, "sqrt");
        typ(&mut h, "alpha");
        let l = lin(&h);
        assert!(l.contains('\u{221A}') && l.contains('\u{3B1}'), "{l}");
    }

    #[test]
    fn arrows_and_backspace_edit_the_tree() {
        let mut h = opened();
        typ(&mut h, "a/b");
        press(&mut h, Key::ArrowUp);
        assert_eq!(st(&h).ed.caret().path, vec![(0, 0)], "Up goes to the numerator");
        press(&mut h, Key::ArrowDown);
        assert_eq!(st(&h).ed.caret().path, vec![(0, 1)]);
        press(&mut h, Key::ArrowLeft);
        press(&mut h, Key::ArrowLeft);
        assert_eq!(st(&h).ed.caret().path, vec![(0, 0)], "Left crosses into the numerator");
        press(&mut h, Key::Home);
        press(&mut h, Key::Backspace);
        assert_eq!(lin(&h), "ab", "Backspace at the start unwraps the fraction");
        press(&mut h, Key::Delete);
        assert_eq!(lin(&h), "b");
        press(&mut h, Key::End);
        press(&mut h, Key::Backspace);
        assert!(st(&h).ed.is_blank());
    }

    #[test]
    fn ctrl_z_and_ctrl_shift_z_undo_and_redo_in_the_editor() {
        let mut h = opened();
        typ(&mut h, "x+a/b");
        press(&mut h, Key::ArrowRight);
        press(&mut h, Key::Backspace);
        assert_eq!(lin(&h), "x+a/b", "the first Backspace only steps inside");
        press(&mut h, Key::Backspace);
        assert_eq!(lin(&h), "x+a/");
        h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
        h.run();
        assert_eq!(lin(&h), "x+a/b", "Ctrl+Z brings the atom back");
        h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);
        h.run();
        assert_eq!(lin(&h), "x+a/");
        h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
        h.run();
        h.key_press_modifiers(Modifiers::COMMAND, Key::Y);
        h.run();
        assert_eq!(lin(&h), "x+a/", "Ctrl+Y redoes too");
    }

    #[test]
    fn tab_stays_in_the_canvas_and_moves_between_places() {
        let mut h = opened();
        typ(&mut h, "a/b");
        press(&mut h, Key::ArrowUp);
        press(&mut h, Key::Tab);
        assert_eq!(st(&h).ed.caret().path, vec![(0, 1)]);
        typ(&mut h, "c");
        assert_eq!(lin(&h), "a/cb", "typing after Tab goes to the tree, not the linear field");
    }

    #[test]
    fn escape_with_unsaved_work_asks_before_discarding() {
        let mut h = opened();
        typ(&mut h, "x^2");
        press(&mut h, Key::Escape);
        assert!(h.state().dialog.is_some(), "first Escape only warns");
        assert!(st(&h).error.is_some());
        press(&mut h, Key::Escape);
        assert!(h.state().dialog.is_none(), "second Escape discards");
        let mut h = opened();
        press(&mut h, Key::Escape);
        assert!(h.state().dialog.is_none(), "nothing typed: closes at once");
    }

    #[test]
    fn shift_arrows_select_and_a_palette_click_wraps_the_selection() {
        let mut h = opened();
        typ(&mut h, "xy");
        h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowLeft);
        h.run();
        h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowLeft);
        h.run();
        assert_eq!(st(&h).ed.selection(), Some((0, 2)));
        h.get_by_label("\u{221A}").click_accesskit();
        h.run();
        h.run();
        assert!(matches!(st(&h).ed.math().body.first(), Some(wordcraft_math::Node::Rad { body, .. }) if wordcraft_math::units(body) == 2));
        typ(&mut h, "z");
        assert_eq!(lin(&h), "\u{221A}xyz");
    }

    #[test]
    fn the_quadratic_formula_types_straight_through() {
        let mut h = opened();
        typ(&mut h, "x=(-b+-sqrt(b^2");
        press(&mut h, Key::ArrowRight);
        typ(&mut h, "-4ac))/2a");
        let l = lin(&h);
        assert!(l.contains('\u{B1}') && l.contains('\u{221A}') && l.ends_with("/2a"), "{l}");
        assert!(matches!(st(&h).ed.math().body.last(), Some(wordcraft_math::Node::Frac { .. })));
    }

    #[test]
    fn typing_survives_a_palette_click_and_lost_focus() {
        let mut h = opened();
        st_mut(&mut h).wants_canvas = true;
        h.get_by_label("a/b").click_accesskit();
        h.run();
        h.run();
        h.get_by_label("x^n").click_accesskit();
        h.run();
        h.run();
        typ(&mut h, "2");
        assert!(lin(&h).contains('2'), "{}", lin(&h));
    }

    #[test]
    fn palette_structures_insert_at_the_caret() {
        let mut h = opened();
        typ(&mut h, "b");
        h.get_by_label("x^n").click_accesskit();
        h.run();
        h.run();
        typ(&mut h, "2");
        assert_eq!(lin(&h), "b^2");
        press(&mut h, Key::ArrowRight);
        h.get_by_label("a/b").click_accesskit();
        h.run();
        h.run();
        assert!(matches!(st(&h).ed.math().body.last(), Some(wordcraft_math::Node::Frac { .. })), "{:?}", st(&h).ed.math().body);
    }

    #[test]
    fn empty_places_show_boxes_and_the_caret_has_a_rect() {
        let mut h = opened();
        typ(&mut h, "a/");
        let s = st(&h);
        assert!(to_linear(&s.ed.display_math()).contains('\u{25A1}'));
        assert!(!to_linear(s.ed.math()).contains('\u{25A1}'));
        let cv = s.canvas.as_ref().expect("canvas");
        assert!(cv.layout.caret_rect(s.ed.caret()).is_some(), "caret has a rectangle inside the empty denominator");
    }

    #[test]
    fn clicking_the_canvas_places_the_caret() {
        let mut h = opened();
        typ(&mut h, "ab+cd");
        let hit = {
            let s = st(&h);
            let cv = s.canvas.as_ref().expect("canvas");
            let r = cv.layout.caret_rect(&Caret { path: vec![], pos: 1 }).expect("rect");
            assert!(r.y1 > r.y0);
            let (o, k) = s.view;
            pos2(o.x + r.x0 as f32 * k, o.y + (r.y0 + r.y1) as f32 * 0.5 * k)
        };
        h.event(egui::Event::PointerMoved(hit));
        h.event(egui::Event::PointerButton { pos: hit, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
        h.run();
        h.event(egui::Event::PointerButton { pos: hit, button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
        h.run();
        assert_eq!(st(&h).ed.caret().pos, 1, "caret between a and b");
        typ(&mut h, "x");
        assert_eq!(lin(&h), "axb+cd");
    }

    #[test]
    fn linear_field_is_still_a_way_in() {
        let mut h = opened();
        h.get_by_role(egui::accesskit::Role::TextInput).click();
        h.run();
        typ(&mut h, "x^2+1");
        h.run();
        assert_eq!(lin(&h), "x^2+1");
        assert!(st(&h).canvas.as_ref().is_some_and(|c| c.shown.body.len() >= 2), "the canvas follows the linear field");
        h.get_by_label("Insert").click_accesskit();
        h.run();
        assert!(h.state().dialog.is_none());
        assert_eq!(eq_count(h.state()), 1);
    }

    #[test]
    fn typed_equation_inserts_with_the_command() {
        let mut h = opened();
        typ(&mut h, "x^2");
        press(&mut h, Key::ArrowRight);
        typ(&mut h, "+1");
        h.get_by_label("Insert").click_accesskit();
        h.run();
        let app = h.state();
        assert_eq!(eq_count(app), 1, "the equation was inserted");
        assert!(app.dialog.is_none(), "the dialog closed");
        let text = app.session.doc.para(wordcraft_doc::StoryRef::Body, &wordcraft_doc::Path(vec![0])).map(|p| p.plain_text()).unwrap_or_default();
        assert!(text.contains('x') && text.contains('2'), "{text:?}");
    }

    #[test]
    fn insert_with_nothing_typed_keeps_the_dialog_open() {
        let mut h = opened();
        h.get_by_label("Insert").click_accesskit();
        h.run();
        assert!(h.state().dialog.is_some());
        assert_eq!(eq_count(h.state()), 0);
    }

    #[test]
    fn the_ribbon_command_opens_the_dialog() {
        let mut a = app();
        a.run("ui.dialog", json!({"name": "equation"})).unwrap();
        assert!(matches!(a.dialog, Some(Dialog::Equation { .. })));
        assert_eq!(eq_count(&a), 0, "opening the dialog inserts nothing");
    }

    #[test]
    fn reedit_loads_the_equation_and_update_replaces_it_in_place() {
        let mut a = app();
        a.run("insert.equation", json!({"linear": "a/b"})).unwrap();
        // The caret sits right after the inserted equation: the dialog opens on it.
        open(&mut a);
        let mut h = harness(a);
        h.run();
        h.run();
        assert!(st(&h).is_edit());
        assert_eq!(st(&h).linear.replace(' ', ""), "a/b");
        press(&mut h, Key::Home);
        press(&mut h, Key::ArrowRight);
        typ(&mut h, "z");
        h.get_by_label("Update").click_accesskit();
        h.run();
        assert!(h.state().dialog.is_none());
        assert_eq!(eq_count(h.state()), 1, "updated, not inserted");
        let omml = h.state_mut().session.run("equation.get", &json!({"index": 0})).unwrap();
        assert!(omml["linear"].as_str().unwrap_or("").contains('z'), "{omml}");
    }

    #[test]
    fn a_caret_away_from_any_equation_opens_a_new_one() {
        let mut a = app();
        a.run("insert.equation", json!({"linear": "a/b"})).unwrap();
        a.run("text.insert", json!({"text": " tail"})).unwrap();
        open(&mut a);
        assert!(matches!(&a.dialog, Some(Dialog::Equation { st }) if !st.is_edit()));
    }

    #[test]
    fn hostile_input_in_the_dialog_does_not_panic() {
        let mut h = opened();
        let keys = [Key::ArrowLeft, Key::ArrowRight, Key::ArrowUp, Key::ArrowDown, Key::Home, Key::End, Key::Backspace, Key::Delete];
        let texts = ["x^", "_", "/", "sqrt", "pi", "(", "2", "^^^"];
        for i in 0..60usize {
            typ(&mut h, texts[i % texts.len()]);
            press(&mut h, keys[(i * 5 + 3) % keys.len()]);
        }
        assert!(st(&h).ed.is_valid());
        assert!(h.state().dialog.is_some());
    }

    #[test]
    fn typed_latex_works_in_the_canvas() {
        let mut h = opened();
        typ(&mut h, "\\alpha");
        assert_eq!(lin(&h), "\u{3b1}");
        typ(&mut h, "+\\frac{1}{2}");
        assert_eq!(lin(&h), "\u{3b1}+1/2");
        typ(&mut h, "\\sqrt{x}");
        assert!(lin(&h).ends_with('x'), "{}", lin(&h));
        assert!(st(&h).ed.math().body.iter().any(|n| matches!(n.bare(), wordcraft_math::Node::Rad { .. })));
    }

    #[test]
    fn typing_lim_with_a_subscript_leaves_no_empty_box() {
        let mut h = opened();
        typ(&mut h, "lim_x");
        assert!(!st(&h).linear.contains("()_"), "{}", st(&h).linear);
        let mut h = opened();
        typ(&mut h, "\\lim_{x\\to0}");
        assert!(!st(&h).linear.contains("()_"), "{}", st(&h).linear);
    }

    #[test]
    fn enter_in_the_canvas_inserts_the_equation() {
        let mut h = opened();
        typ(&mut h, "x^2");
        press(&mut h, Key::Enter);
        h.run();
        assert!(h.state().dialog.is_none());
        assert_eq!(eq_count(h.state()), 1);
        // Nothing typed: Enter keeps the dialog open with a message.
        let mut h = opened();
        press(&mut h, Key::Enter);
        assert!(h.state().dialog.is_some());
        assert_eq!(eq_count(h.state()), 0);
    }

    #[test]
    fn alt_equals_opens_the_editor_instead_of_dropping_a_canned_equation() {
        let mut h = Harness::builder().with_size(vec2(1100.0, 800.0)).build_ui_state(
            |ui, app: &mut WordApp| {
                app.logic(ui.ctx());
                app.ui(ui);
            },
            app(),
        );
        for _ in 0..3 {
            h.step();
        }
        h.key_press_modifiers(Modifiers::ALT, Key::Equals);
        for _ in 0..3 {
            h.step();
        }
        assert!(matches!(h.state().dialog, Some(Dialog::Equation { .. })), "dialog opens");
        assert_eq!(eq_count(h.state()), 0, "nothing inserted behind the dialog");
        // A second press while the editor is open does not reset it.
        typ_state(&mut h, "x");
        h.key_press_modifiers(Modifiers::ALT, Key::Equals);
        h.step();
        h.step();
        assert!(matches!(&h.state().dialog, Some(Dialog::Equation { st }) if !st.ed.is_blank()));
    }

    fn typ_state(h: &mut Harness<'static, WordApp>, text: &str) {
        h.event(egui::Event::Text(text.into()));
        h.step();
        h.step();
    }

    #[test]
    fn shift_click_extends_the_selection() {
        let mut h = opened();
        typ(&mut h, "abcd");
        let (origin, k) = st(&h).view;
        let at = |h: &Harness<'static, WordApp>, pos: usize| {
            let r = st(h).canvas.as_ref().and_then(|c| c.layout.caret_rect(&Caret { path: vec![], pos }));
            r.map(|r| pos2(origin.x + ((r.x0 + r.x1) / 2.0) as f32 * k, origin.y + ((r.y0 + r.y1) / 2.0) as f32 * k))
        };
        let (Some(p1), Some(p3)) = (at(&h, 1), at(&h, 3)) else { panic!("no caret rects") };
        h.event(egui::Event::PointerMoved(p1));
        h.run();
        h.event(egui::Event::PointerButton { pos: p1, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
        h.run();
        h.event(egui::Event::PointerButton { pos: p1, button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
        h.run();
        h.event(egui::Event::PointerMoved(p3));
        h.run();
        let down = egui::Event::PointerButton { pos: p3, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::SHIFT };
        h.event_modifiers(down, Modifiers::SHIFT);
        h.run();
        let up = egui::Event::PointerButton { pos: p3, button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::SHIFT };
        h.event_modifiers(up, Modifiers::SHIFT);
        h.run();
        assert_eq!(st(&h).ed.selection(), Some((1, 3)), "{:?}", st(&h));
    }

    #[test]
    fn double_clicking_an_equation_in_the_page_opens_it_for_editing() {
        let mut a = app();
        a.run("text.insert", json!({"text": "Before "})).unwrap();
        a.run("insert.equation", json!({"linear": "a/b"})).unwrap();
        a.run("text.insert", json!({"text": " after"})).unwrap();
        let mut h = Harness::builder().with_size(vec2(1100.0, 800.0)).with_step_dt(0.04).build_ui_state(
            |ui, app: &mut WordApp| {
                app.logic(ui.ctx());
                app.ui(ui);
            },
            a,
        );
        for _ in 0..6 {
            h.step();
        }
        // The middle of the equation, on page 0.
        let click = {
            let app = h.state_mut();
            let layout = app.session.layout().clone();
            let off = "Before ".len();
            let pos = |o: usize| wordcraft_doc::Pos { story: wordcraft_doc::StoryRef::Body, path: wordcraft_doc::Path(vec![0]), off: o };
            let (c0, c1) =
                (layout.caret(&pos(off)).expect("caret before"), layout.caret(&pos(off + wordcraft_doc::para::OBJ.len_utf8())).expect("caret after"));
            let r = app.canvas.page_rects.get(c0.page).copied().expect("page rect");
            let k = app.canvas.scale;
            pos2(r.min.x + (c0.x + c1.x) * 0.5 * k, r.min.y + (c0.top + c0.height * 0.5) * k)
        };
        h.event(egui::Event::PointerMoved(click));
        for _ in 0..2 {
            h.event(egui::Event::PointerButton { pos: click, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
            h.step();
            h.event(egui::Event::PointerButton { pos: click, button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
            h.step();
        }
        for _ in 0..4 {
            h.step();
        }
        assert!(matches!(&h.state().dialog, Some(Dialog::Equation { st }) if st.is_edit()), "{:?}", h.state().dialog.as_ref().map(Dialog::name));
    }

    fn eq_linear(app: &mut WordApp) -> String {
        app.session.run("equation.get", &json!({"index": 0})).ok().and_then(|v| v["linear"].as_str().map(str::to_string)).unwrap_or_default()
    }

    #[test]
    fn typing_edits_the_equation_in_the_page_at_once() {
        let mut h = opened();
        assert_eq!(eq_count(h.state()), 0);
        typ(&mut h, "x^2");
        assert_eq!(eq_count(h.state()), 1, "the page shows the equation while the dialog is open");
        assert!(h.state().dialog.is_some());
        press(&mut h, Key::ArrowRight);
        typ(&mut h, "+1");
        assert_eq!(eq_count(h.state()), 1, "still one equation, updated in place");
        assert!(eq_linear(h.state_mut()).contains('1'));
        let sel = h.state().session.sel.clone();
        assert!(!sel.is_collapsed(), "the equation is selected on the page");
        h.get_by_label("Insert").click_accesskit();
        h.run();
        assert!(h.state().dialog.is_none());
        assert_eq!(eq_count(h.state()), 1);
        assert!(h.state().session.sel.is_collapsed(), "the caret goes after the equation");
        // The whole editing session is one undo step.
        assert!(h.state_mut().session.undo());
        assert_eq!(eq_count(h.state()), 0);
    }

    #[test]
    fn cancel_and_escape_take_the_live_edit_back() {
        let mut h = opened();
        typ(&mut h, "x^2");
        press(&mut h, Key::ArrowRight);
        typ(&mut h, "+1");
        h.get_by_label("Cancel").click_accesskit();
        h.run();
        assert!(h.state().dialog.is_none());
        assert_eq!(eq_count(h.state()), 0, "Cancel removes the inserted equation");
        let mut h = opened();
        typ(&mut h, "x^2");
        press(&mut h, Key::Escape);
        assert_eq!(eq_count(h.state()), 1, "the first Escape only warns");
        press(&mut h, Key::Escape);
        assert!(h.state().dialog.is_none());
        assert_eq!(eq_count(h.state()), 0);
    }

    #[test]
    fn cancelling_a_reedit_restores_the_original() {
        let mut a = app();
        a.run("insert.equation", json!({"linear": "a/b"})).unwrap();
        let before = eq_linear(&mut a);
        open(&mut a);
        let mut h = harness(a);
        h.run();
        h.run();
        press(&mut h, Key::Home);
        typ(&mut h, "z");
        assert_ne!(eq_linear(h.state_mut()), before, "the page follows the edit");
        h.get_by_label("Cancel").click_accesskit();
        h.run();
        assert_eq!(eq_count(h.state()), 1);
        assert_eq!(eq_linear(h.state_mut()), before);
        // The earlier insert is still one undo step away, not consumed by Cancel.
        assert!(h.state().session.can_undo());
    }

    #[test]
    fn track_changes_leaves_the_page_alone_until_insert() {
        let mut a = app();
        a.run("review.trackChanges", json!({"on": true})).ok();
        a.session.doc.settings.track_changes = true;
        open(&mut a);
        let mut h = harness(a);
        h.run();
        h.run();
        typ(&mut h, "x^2");
        assert_eq!(eq_count(h.state()), 0);
        h.get_by_label("Insert").click_accesskit();
        h.run();
        assert_eq!(eq_count(h.state()), 1);
    }
}
