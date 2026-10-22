//! A GTK terminal widget: libghostty-vt does the emulation, this draws its
//! frames with Pango and turns GDK key events into terminal input.

use std::cell::{Cell as StdCell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};

use super::ffi::*;
use super::keys;
use super::vt::{Frame, KeyInput, Rgb, Vt};

const DEFAULT_FONT: &str = "Monospace 11";
const SCROLL_LINES: f64 = 3.0;

type InputHandler = Box<dyn Fn(&[u8])>;
type ResizeHandler = Box<dyn Fn(u16, u16)>;

struct Metrics {
    font: pango::FontDescription,
    width: f64,
    height: f64,
}

struct State {
    vt: Vt,
    metrics: Metrics,
    size: (u16, u16),
}

/// The terminal widget. Program output goes in through [`TerminalView::feed`];
/// keystrokes and size changes come out through the connected handlers.
#[derive(Clone)]
pub struct TerminalView {
    area: gtk::DrawingArea,
    state: Rc<RefCell<State>>,
    on_input: Rc<RefCell<Option<InputHandler>>>,
    on_resize: Rc<RefCell<Option<ResizeHandler>>>,
}

impl TerminalView {
    pub fn new() -> Self {
        let area = gtk::DrawingArea::builder()
            .focusable(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        area.add_css_class("terminal");

        let on_input: Rc<RefCell<Option<InputHandler>>> = Rc::default();
        let replies = on_input.clone();
        let vt = Vt::new(80, 24, move |bytes| {
            if let Some(handler) = replies.borrow().as_ref() {
                handler(bytes);
            }
        });
        let metrics = metrics(&area);
        let view = Self {
            area,
            state: Rc::new(RefCell::new(State {
                vt,
                metrics,
                size: (80, 24),
            })),
            on_input,
            on_resize: Rc::default(),
        };
        view.follow_style();
        view.setup_drawing();
        view.setup_input();
        view
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    /// Current size in cells.
    pub fn size(&self) -> (u16, u16) {
        self.state.borrow().size
    }

    pub fn connect_input(&self, handler: impl Fn(&[u8]) + 'static) {
        self.on_input.replace(Some(Box::new(handler)));
    }

    pub fn connect_resize(&self, handler: impl Fn(u16, u16) + 'static) {
        self.on_resize.replace(Some(Box::new(handler)));
    }

    /// Feeds program output to the terminal.
    pub fn feed(&self, bytes: &[u8]) {
        self.state.borrow_mut().vt.write(bytes);
        self.area.queue_draw();
    }

    fn send(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.state.borrow_mut().vt.scroll_to_bottom();
        if let Some(handler) = self.on_input.borrow().as_ref() {
            handler(bytes);
        }
        self.area.queue_draw();
    }

    /// Uses libadwaita's light or dark window colours as the terminal
    /// defaults, so the console matches the rest of the app.
    fn follow_style(&self) {
        // The style manager outlives this widget, so hold it weakly.
        let style = adw::StyleManager::default();
        let state = Rc::downgrade(&self.state);
        let area = self.area.downgrade();
        let apply = move |style: &adw::StyleManager| {
            let (Some(state), Some(area)) = (state.upgrade(), area.upgrade()) else {
                return;
            };
            let (fg, bg) = if style.is_dark() {
                (rgb(0xff, 0xff, 0xff), rgb(0x1e, 0x1e, 0x1e))
            } else {
                (rgb(0x1e, 0x1e, 0x1e), rgb(0xff, 0xff, 0xff))
            };
            state.borrow_mut().vt.set_default_colors(fg, bg);
            area.queue_draw();
        };
        apply(&style);
        let handler = StdCell::new(Some(style.connect_dark_notify(apply)));
        self.area.connect_destroy(move |_| {
            if let Some(handler) = handler.take() {
                style.disconnect(handler);
            }
        });
    }

    fn setup_drawing(&self) {
        let state = self.state.clone();
        self.area.set_draw_func(move |area, cr, width, height| {
            let mut state = state.borrow_mut();
            let frame = state.vt.frame();
            draw(
                area,
                cr,
                width,
                height,
                &frame,
                &state.metrics,
                area.has_focus(),
            );
        });

        let view = self.clone();
        self.area.connect_resize(move |_, width, height| {
            let (cols, rows, changed) = {
                let mut state = view.state.borrow_mut();
                let cols = ((width as f64 / state.metrics.width).floor() as u16).max(1);
                let rows = ((height as f64 / state.metrics.height).floor() as u16).max(1);
                let changed = state.size != (cols, rows);
                if changed {
                    let (cw, ch) = (state.metrics.width as u32, state.metrics.height as u32);
                    state.vt.resize(cols, rows, cw, ch);
                    state.size = (cols, rows);
                }
                (cols, rows, changed)
            };
            if changed && let Some(handler) = view.on_resize.borrow().as_ref() {
                handler(cols, rows);
            }
        });
    }

    fn setup_input(&self) {
        let im = gtk::IMMulticontext::new();
        im.set_client_widget(Some(&self.area));
        let view = self.clone();
        im.connect_commit(move |_, text| view.send(text.as_bytes()));

        let keys = gtk::EventControllerKey::new();
        keys.set_im_context(Some(&im));
        let view = self.clone();
        keys.connect_key_pressed(move |controller, keyval, keycode, state| {
            view.key_pressed(controller, keyval, keycode, state)
        });
        self.area.add_controller(keys);

        let focus = gtk::EventControllerFocus::new();
        let (area, im_in) = (self.area.clone(), im.clone());
        focus.connect_enter(move |_| {
            im_in.focus_in();
            area.queue_draw();
        });
        let area = self.area.clone();
        focus.connect_leave(move |_| {
            im.focus_out();
            area.queue_draw();
        });
        self.area.add_controller(focus);

        let click = gtk::GestureClick::new();
        let area = self.area.clone();
        click.connect_pressed(move |_, _, _, _| {
            area.grab_focus();
        });
        self.area.add_controller(click);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let view = self.clone();
        let pending = StdCell::new(0.0);
        scroll.connect_scroll(move |_, _, dy| {
            let lines = pending.get() + dy * SCROLL_LINES;
            let whole = lines.trunc();
            pending.set(lines - whole);
            if whole != 0.0 {
                view.state.borrow_mut().vt.scroll(whole as isize);
                view.area.queue_draw();
            }
            glib::Propagation::Stop
        });
        self.area.add_controller(scroll);
    }

    fn key_pressed(
        &self,
        controller: &gtk::EventControllerKey,
        keyval: gdk::Key,
        keycode: u32,
        state: gdk::ModifierType,
    ) -> glib::Propagation {
        let ctrl_shift = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK;
        if state.contains(ctrl_shift) && matches!(keyval, gdk::Key::V | gdk::Key::v) {
            self.paste();
            return glib::Propagation::Stop;
        }

        let key = keys::from_xkb_keycode(keycode);
        let event = controller.current_event();
        let consumed = event
            .as_ref()
            .and_then(|e| e.downcast_ref::<gdk::KeyEvent>())
            .map_or(gdk::ModifierType::empty(), |e| e.consumed_modifiers());
        let layout = event
            .as_ref()
            .and_then(|e| e.downcast_ref::<gdk::KeyEvent>())
            .map_or(0, |e| e.layout());

        let text = keyval
            .to_unicode()
            .filter(|c| !c.is_control())
            .map(String::from);
        let unshifted = self
            .area
            .display()
            .map_keycode(keycode)
            .and_then(|entries| {
                entries
                    .into_iter()
                    .find(|(k, _)| k.group() as u32 == layout && k.level() == 0)
            })
            .and_then(|(_, key)| key.to_unicode());

        if key == GHOSTTY_KEY_UNIDENTIFIED && text.is_none() {
            return glib::Propagation::Proceed;
        }

        let bytes = self.state.borrow_mut().vt.encode_key(KeyInput {
            key,
            mods: mods(state),
            consumed_mods: mods(consumed),
            text: text.as_deref(),
            unshifted,
        });
        if bytes.is_empty() {
            return glib::Propagation::Proceed;
        }
        self.send(&bytes);
        glib::Propagation::Stop
    }

    fn paste(&self) {
        let view = self.clone();
        self.area
            .clipboard()
            .read_text_async(None::<&gio::Cancellable>, move |text| {
                if let Ok(Some(text)) = text {
                    // Line endings become carriage returns, as a typed Enter would.
                    view.send(text.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
                }
            });
    }
}

impl Default for TerminalView {
    fn default() -> Self {
        Self::new()
    }
}

fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

fn mods(state: gdk::ModifierType) -> GhosttyMods {
    let mut mods = 0;
    for (mask, bit) in [
        (gdk::ModifierType::SHIFT_MASK, GHOSTTY_MODS_SHIFT),
        (gdk::ModifierType::CONTROL_MASK, GHOSTTY_MODS_CTRL),
        (gdk::ModifierType::ALT_MASK, GHOSTTY_MODS_ALT),
        (gdk::ModifierType::SUPER_MASK, GHOSTTY_MODS_SUPER),
        (gdk::ModifierType::LOCK_MASK, GHOSTTY_MODS_CAPS_LOCK),
    ] {
        if state.contains(mask) {
            mods |= bit;
        }
    }
    mods as GhosttyMods
}

/// The desktop's monospace font, falling back to fontconfig's "Monospace".
fn font() -> pango::FontDescription {
    let name = gio::SettingsSchemaSource::default()
        .and_then(|source| source.lookup("org.gnome.desktop.interface", true))
        .filter(|schema| schema.has_key("monospace-font-name"))
        .map(|_| {
            gio::Settings::new("org.gnome.desktop.interface")
                .string("monospace-font-name")
                .to_string()
        })
        .unwrap_or_else(|| DEFAULT_FONT.to_owned());
    pango::FontDescription::from_string(&name)
}

fn metrics(area: &gtk::DrawingArea) -> Metrics {
    let font = font();
    let context = area.pango_context();
    let metrics = context.metrics(Some(&font), None);
    let scale = pango::SCALE as f64;
    Metrics {
        width: (metrics.approximate_digit_width() as f64 / scale)
            .ceil()
            .max(1.0),
        height: ((metrics.ascent() + metrics.descent()) as f64 / scale)
            .ceil()
            .max(1.0),
        font,
    }
}

fn set_source(cr: &gtk::cairo::Context, color: Rgb, alpha: f64) {
    cr.set_source_rgba(
        color.r as f64 / 255.0,
        color.g as f64 / 255.0,
        color.b as f64 / 255.0,
        alpha,
    );
}

fn draw(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    frame: &Frame,
    metrics: &Metrics,
    focused: bool,
) {
    let (cw, ch) = (metrics.width, metrics.height);
    set_source(cr, frame.background, 1.0);
    cr.rectangle(0.0, 0.0, width as f64, height as f64);
    let _ = cr.fill();

    let layout = area.create_pango_layout(None);
    for (y, row) in frame.rows.iter().enumerate() {
        let top = y as f64 * ch;
        for (x, cell) in row.iter().enumerate() {
            let left = x as f64 * cw;
            if let Some(bg) = cell.bg {
                set_source(cr, bg, 1.0);
                cr.rectangle(left, top, cw, ch);
                let _ = cr.fill();
            }
            if cell.text.is_empty() || cell.text == " " {
                continue;
            }

            let mut font = metrics.font.clone();
            if cell.bold {
                font.set_weight(pango::Weight::Bold);
            }
            if cell.italic {
                font.set_style(pango::Style::Italic);
            }
            layout.set_font_description(Some(&font));
            layout.set_text(&cell.text);
            let attrs = pango::AttrList::new();
            if cell.underline {
                attrs.insert(pango::AttrInt::new_underline(pango::Underline::Single));
            }
            if cell.strikethrough {
                attrs.insert(pango::AttrInt::new_strikethrough(true));
            }
            layout.set_attributes(Some(&attrs));

            set_source(cr, cell.fg, if cell.faint { 0.6 } else { 1.0 });
            cr.move_to(left, top);
            pangocairo::functions::show_layout(cr, &layout);
        }
    }

    if let Some((x, y)) = frame.cursor {
        let (left, top) = (x as f64 * cw, y as f64 * ch);
        set_source(cr, frame.cursor_color, 1.0);
        if focused {
            cr.rectangle(left, top, cw, ch);
            let _ = cr.fill();
            // Redraw the character under a block cursor in the background colour.
            if let Some(cell) = frame.rows.get(y as usize).and_then(|r| r.get(x as usize))
                && !cell.text.trim().is_empty()
            {
                layout.set_font_description(Some(&metrics.font));
                layout.set_attributes(None);
                layout.set_text(&cell.text);
                set_source(cr, frame.background, 1.0);
                cr.move_to(left, top);
                pangocairo::functions::show_layout(cr, &layout);
            }
        } else {
            cr.set_line_width(1.0);
            cr.rectangle(left + 0.5, top + 0.5, cw - 1.0, ch - 1.0);
            let _ = cr.stroke();
        }
    }
}
