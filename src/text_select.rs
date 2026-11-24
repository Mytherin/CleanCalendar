//! Double- and triple-click selection in text fields, done the native way.
//!
//! egui selects a word only when the second click is *released*, and counts
//! clicks with its own 0.3 s window measured release to release. Native text
//! fields select on the second *press*, use the OS double-click time (500 ms
//! by default on Windows), and keep the word (or line) selected while the
//! button is held and dragged on. Text fields added with [`add`] behave like
//! that. Where egui has already drawn a different selection that frame, the
//! frame is redone before it's shown, so its selection never flashes.

use eframe::egui::{self, text::CCursor, text::CCursorRange, Context, Id, PointerButton, Pos2, Response, TextEdit, Ui};

/// How far apart (in points) the presses of a multi-click may be.
const SLOP: f32 = 6.0;

/// The OS double-click time, in seconds.
pub fn double_click_time() -> f64 {
    #[cfg(windows)]
    {
        // SAFETY: takes no arguments and only reads a system setting.
        let ms = unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() };
        if ms > 0 {
            return f64::from(ms) / 1000.0;
        }
    }
    0.5
}

/// Makes egui count double clicks (in lists, etc.) with the OS timing too.
pub fn install(ctx: &Context) {
    ctx.options_mut(|o| o.input_options.max_double_click_delay = double_click_time());
}

#[derive(Clone, Copy, Default)]
struct Presses {
    time: f64,
    pos: Pos2,
    count: u32,
    /// Where the primary button went down this frame, and the how-manyth
    /// click of a series that press is.
    this_frame: Option<(Pos2, u32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    Word,
    Line,
}

/// What a double or triple press selected, as char indices: it stays
/// selected while the button is down, also when dragging on from it.
#[derive(Clone, Copy)]
struct Anchor {
    start: usize,
    end: usize,
    /// Where the press was, to tell a drag from a still button.
    press: Pos2,
}

fn presses_id() -> Id {
    Id::new(module_path!())
}

/// Counts this frame's presses. Call once per frame, before any UI.
pub fn begin_frame(ctx: &Context) {
    let delay = ctx.options(|o| o.input_options.max_double_click_delay);
    let mut st: Presses = ctx.data(|d| d.get_temp(presses_id())).unwrap_or_default();
    st.this_frame = None;
    ctx.input(|i| {
        for event in &i.events {
            if let egui::Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, .. } = event {
                let again = st.count > 0 && i.time - st.time <= delay && st.pos.distance(*pos) <= SLOP;
                st.count = if again { st.count + 1 } else { 1 };
                st.time = i.time;
                st.pos = *pos;
                st.this_frame = Some((*pos, st.count));
            }
        }
    });
    ctx.data_mut(|d| d.insert_temp(presses_id(), st));
}

/// Adds a text field whose double/triple clicks select words/lines on the press.
pub fn add(ui: &mut Ui, edit: TextEdit) -> Response {
    let mut out = edit.show(ui);
    #[cfg(test)]
    frame_tests::DRAWN.with(|d| d.set(range(&out.state).map(|(a, b)| (a.min(b), a.max(b)))));
    let ctx = ui.ctx().clone();
    let id = out.response.id;
    let anchor_key = id.with("text-select-anchor");
    let last_key = id.with("text-select-last");
    let mut anchor: Option<Anchor> = ctx.data(|d| d.get_temp(anchor_key));

    let presses: Presses = ctx.data(|d| d.get_temp(presses_id())).unwrap_or_default();
    if let Some((pos, count)) = presses.this_frame {
        anchor = None;
        let resp = &out.response;
        if count >= 2 && resp.sense.click && resp.hovered() && resp.interact_rect.contains(pos) {
            let at = out.galley.cursor_from_pos(pos - out.galley_pos).ccursor.index;
            let unit = if count == 2 { Unit::Word } else { Unit::Line };
            let (start, end) = unit_at(out.galley.text(), at, unit);
            anchor = Some(Anchor { start, end, press: pos });
        }
    }

    let (down, released, pointer) =
        ctx.input(|i| (i.pointer.primary_down(), i.pointer.primary_released(), i.pointer.interact_pos()));
    match anchor {
        // Applied on the release frame too, overriding egui's own (slower,
        // differently timed) word or line selection on release.
        Some(a) if down || released => {
            let (mut secondary, mut primary) = (a.start, a.end);
            // Dragging on from the word or line: it stays selected, and the
            // selection extends to the pointer like egui's own drag does
            // (egui moves that end to the pointer every frame anyway).
            if let Some(pos) = pointer.filter(|p| p.distance(a.press) > SLOP) {
                let at = out.galley.cursor_from_pos(pos - out.galley_pos).ccursor.index;
                if at < a.start {
                    (secondary, primary) = (a.end, at);
                } else if at > a.end {
                    primary = at;
                }
            }
            set_range(&ctx, &mut out, (secondary, primary));
            ctx.data_mut(|d| if released { d.remove::<Anchor>(anchor_key) } else { d.insert_temp(anchor_key, a) });
        }
        _ => {
            ctx.data_mut(|d| d.remove::<Anchor>(anchor_key));
            // egui counts any clicks in quick succession as a double or
            // triple click, wherever they are: the first click on another
            // word just after a double click would select a word or line.
            // To us that's a single click, so undo it.
            let egui_multi = out.response.double_clicked() || out.response.triple_clicked();
            if egui_multi && presses.count < 2 {
                if let Some(before) = ctx.data(|d| d.get_temp::<(usize, usize)>(last_key)) {
                    set_range(&ctx, &mut out, before);
                }
            }
        }
    }
    if let Some(r) = range(&out.state) {
        ctx.data_mut(|d| d.insert_temp(last_key, r));
    }
    out.response
}

/// The selection as (secondary, primary) char indices.
fn range(state: &egui::text_edit::TextEditState) -> Option<(usize, usize)> {
    state.cursor.char_range().map(|r| (r.secondary.index, r.primary.index))
}

/// Sets the selection (secondary, primary). If egui drew another one this
/// frame, the frame is run again before it's shown so that never flashes.
fn set_range(ctx: &Context, out: &mut egui::text_edit::TextEditOutput, (secondary, primary): (usize, usize)) {
    if range(&out.state) != Some((secondary, primary)) {
        out.state.cursor.set_char_range(Some(CCursorRange {
            primary: CCursor::new(primary),
            secondary: CCursor::new(secondary),
        }));
        out.state.clone().store(ctx, out.response.id);
        ctx.request_discard("text selection changed after drawing");
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Newline,
    Other,
}

fn class(c: char) -> Class {
    match c {
        '\n' => Class::Newline,
        c if c.is_alphanumeric() || c == '_' => Class::Word,
        c if c.is_whitespace() => Class::Space,
        _ => Class::Other,
    }
}

/// The word or line around char index `at`, as a char range. A word is a run
/// of letters/digits, or of spaces; any other character is a word by itself.
fn unit_at(text: &str, at: usize, unit: Unit) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    let at = at.min(chars.len());
    if unit == Unit::Line {
        let start = chars[..at].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
        let end = chars[at..].iter().position(|&c| c == '\n').map_or(chars.len(), |i| at + i);
        return (start, end);
    }
    // Clicking past the end of a line picks the character before it.
    let i = if at == chars.len() || chars[at] == '\n' {
        match at.checked_sub(1) {
            Some(j) if chars[j] != '\n' => j,
            _ => return (at, at),
        }
    } else {
        at
    };
    let cls = class(chars[i]);
    if cls == Class::Other {
        return (i, i + 1);
    }
    let start = chars[..i].iter().rposition(|&c| class(c) != cls).map_or(0, |j| j + 1);
    let end = chars[i..].iter().position(|&c| class(c) != cls).map_or(chars.len(), |j| i + j);
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        let t = "Hekelveld 10, 1012 SN";
        assert_eq!(unit_at(t, 3, Unit::Word), (0, 9));
        assert_eq!(unit_at(t, 9, Unit::Word), (9, 10)); // the space
        assert_eq!(unit_at(t, 12, Unit::Word), (12, 13)); // the comma
        assert_eq!(unit_at(t, t.chars().count(), Unit::Word), (19, 21)); // past the end
        assert_eq!(unit_at("", 0, Unit::Word), (0, 0));
    }

    #[test]
    fn multiline_location_on_one_line() {
        use crate::model::one_line;
        assert_eq!(
            one_line("CIMA Rooftop Bar\nHekelveld 10, 1012 SN Amsterdam, Netherlands"),
            "CIMA Rooftop Bar, Hekelveld 10, 1012 SN Amsterdam, Netherlands"
        );
        assert_eq!(one_line("A,\r\n\r\n B "), "A, B");
        assert_eq!(one_line("plain"), "plain");
    }

    #[test]
    fn lines() {
        let t = "CIMA Rooftop Bar\nHekelveld 10";
        assert_eq!(unit_at(t, 3, Unit::Line), (0, 16));
        assert_eq!(unit_at(t, 20, Unit::Line), (17, 29));
        assert_eq!(unit_at(t, 16, Unit::Word), (13, 16)); // end of the first line
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;
    thread_local! { pub static DRAWN: std::cell::Cell<Option<(usize, usize)>> = const { std::cell::Cell::new(None) }; }
    use eframe::egui::{Event, Modifiers, RawInput, Rect, Vec2};

    /// Runs frames of a field holding `text`, feeding `events` at `times`,
    /// and returns the selection shown at the end of each frame.
    fn run(text: &str, frames: &[(f64, Vec<Event>)]) -> Vec<Option<(usize, usize)>> {
        let ctx = Context::default();
        install(&ctx);
        let mut text = text.to_owned();
        let id = Id::new("field");
        let mut shown = Vec::new();
        for (time, events) in frames {
            let input = RawInput {
                time: Some(*time),
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 200.0))),
                events: events.clone(),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                begin_frame(ctx);
                egui::CentralPanel::default().show(ctx, |ui| {
                    add(ui, TextEdit::singleline(&mut text).id(id).desired_width(300.0));
                });
            });
            shown.push(DRAWN.with(|d| d.get()));
        }
        shown
    }

    fn button(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
    }

    /// Where the char after `prefix` is drawn in the test field.
    fn at(prefix: &str) -> Pos2 {
        let ctx = Context::default();
        let _ = ctx.run(RawInput::default(), |_| {});
        let x = ctx.fonts(|f| f.layout_no_wrap(prefix.into(), egui::FontId::default(), egui::Color32::WHITE).size().x);
        Pos2::new(8.0 + x + 2.0, 18.0)
    }

    /// Moving to `p` and clicking it at `t` (press, then release 50 ms later).
    fn click(t: f64, p: Pos2) -> [(f64, Vec<Event>); 3] {
        [(t, vec![Event::PointerMoved(p), button(p, true)]), (t + 0.016, vec![]), (t + 0.05, vec![button(p, false)])]
    }

    #[test]
    fn double_click_middle_word() {
        let p = at("CIMA Roo");
        let mut frames = vec![(0.0, vec![Event::PointerMoved(p)])];
        // An earlier click: not part of the double click, but egui counts it.
        frames.extend(click(0.3, p));
        frames.extend(click(1.0, p));
        frames.extend(click(1.2, p));
        frames.push((1.3, vec![]));
        let shown = run("CIMA Rooftop Bar", &frames);
        // From the second press on, only the word is ever drawn.
        assert_eq!(shown[7..], vec![Some((5, 12)); 4][..]);
    }

    #[test]
    fn double_click_one_word_then_another() {
        let (cima, rooftop) = (at("CI"), at("CIMA Roo"));
        let mut frames = vec![(0.0, vec![Event::PointerMoved(cima)])];
        frames.extend(click(0.5, cima));
        frames.extend(click(0.7, cima));
        frames.extend(click(0.9, rooftop));
        frames.extend(click(1.1, rooftop));
        frames.push((1.2, vec![]));
        let shown = run("CIMA Rooftop Bar", &frames);
        assert_eq!(shown[4..7], vec![Some((0, 4)); 3][..], "CIMA");
        // The first click on Rooftop just moves the cursor there...
        assert!(shown[7..10].iter().all(|s| s.is_some_and(|(a, b)| a == b && (5..=12).contains(&a))), "{shown:?}");
        // ...and the second selects it, with nothing else drawn in between.
        assert_eq!(shown[10..], vec![Some((5, 12)); 4][..]);
    }
}
