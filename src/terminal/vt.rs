//! Safe wrapper around a libghostty-vt terminal, its render state and its
//! key encoder.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;

use super::ffi::*;

pub type Rgb = GhosttyColorRgb;

/// One cell of a rendered frame, with colours already resolved.
#[derive(Debug, Clone)]
pub struct Cell {
    pub text: String,
    pub fg: Rgb,
    /// `None` when the cell uses the terminal background.
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub background: Rgb,
    pub rows: Vec<Vec<Cell>>,
    /// Cursor cell, when it is visible in the viewport.
    pub cursor: Option<(u16, u16)>,
    pub cursor_color: Rgb,
}

#[derive(Debug, Clone, Copy)]
pub struct KeyInput<'a> {
    pub key: GhosttyKey,
    pub mods: GhosttyMods,
    pub consumed_mods: GhosttyMods,
    pub text: Option<&'a str>,
    pub unshifted: Option<char>,
}

type PtyWriter = Box<dyn Fn(&[u8])>;

pub struct Vt {
    terminal: GhosttyTerminal,
    render: GhosttyRenderState,
    row_iter: GhosttyRenderStateRowIterator,
    cells: GhosttyRenderStateRowCells,
    encoder: GhosttyKeyEncoder,
    event: GhosttyKeyEvent,
    // Boxed twice so the userdata pointer handed to C stays put.
    _writer: Box<PtyWriter>,
}

macro_rules! check {
    ($call:expr) => {{
        let result = unsafe { $call };
        assert_eq!(
            result,
            GHOSTTY_SUCCESS,
            "{} failed: {result}",
            stringify!($call)
        );
    }};
}

/// Zeroed sized struct with its `size` field set, like GHOSTTY_INIT_SIZED.
macro_rules! sized {
    ($ty:ty) => {{
        let mut value = <$ty>::default();
        value.size = size_of::<$ty>();
        value
    }};
}

unsafe extern "C" fn on_write_pty(
    _terminal: GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    if userdata.is_null() || data.is_null() {
        return;
    }
    // SAFETY: userdata is the `Box<PtyWriter>` owned by the `Vt` whose
    // terminal is calling us, and `data` is valid for `len` bytes.
    let writer = unsafe { &*(userdata as *const PtyWriter) };
    writer(unsafe { std::slice::from_raw_parts(data, len) });
}

impl Vt {
    /// Creates a terminal. `write_pty` receives replies the terminal sends
    /// back to the program (device status reports, mode queries, ...).
    pub fn new(cols: u16, rows: u16, write_pty: impl Fn(&[u8]) + 'static) -> Self {
        let mut terminal = ptr::null_mut();
        let mut render = ptr::null_mut();
        let mut row_iter = ptr::null_mut();
        let mut cells = ptr::null_mut();
        let mut encoder = ptr::null_mut();
        let mut event = ptr::null_mut();
        check!(ghostty_terminal_new(ptr::null(), &mut terminal, cols, rows));
        check!(ghostty_render_state_new(ptr::null(), &mut render));
        check!(ghostty_render_state_row_iterator_new(
            ptr::null(),
            &mut row_iter
        ));
        check!(ghostty_render_state_row_cells_new(ptr::null(), &mut cells));
        check!(ghostty_key_encoder_new(ptr::null(), &mut encoder));
        check!(ghostty_key_event_new(ptr::null(), &mut event));

        let writer: Box<PtyWriter> = Box::new(Box::new(write_pty));
        let userdata = &*writer as *const PtyWriter as *const c_void;
        let callback: GhosttyTerminalWritePtyFn = Some(on_write_pty);
        check!(ghostty_terminal_set(
            terminal,
            GHOSTTY_TERMINAL_OPT_USERDATA,
            userdata
        ));
        check!(ghostty_terminal_set(
            terminal,
            GHOSTTY_TERMINAL_OPT_WRITE_PTY,
            callback.map_or(ptr::null(), |f| f as *const c_void)
        ));

        Self {
            terminal,
            render,
            row_iter,
            cells,
            encoder,
            event,
            _writer: writer,
        }
    }

    pub fn set_default_colors(&mut self, foreground: Rgb, background: Rgb) {
        check!(ghostty_terminal_set(
            self.terminal,
            GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND,
            &foreground as *const Rgb as *const c_void
        ));
        check!(ghostty_terminal_set(
            self.terminal,
            GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
            &background as *const Rgb as *const c_void
        ));
    }

    /// Feeds program output through the VT parser.
    pub fn write(&mut self, bytes: &[u8]) {
        unsafe { ghostty_terminal_vt_write(self.terminal, bytes.as_ptr(), bytes.len()) };
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_width: u32, cell_height: u32) {
        check!(ghostty_terminal_resize(
            self.terminal,
            cols.max(1),
            rows.max(1),
            cell_width,
            cell_height
        ));
    }

    /// Scrolls the viewport by `delta` rows (negative is up).
    pub fn scroll(&mut self, delta: isize) {
        let mut behavior = GhosttyTerminalScrollViewport {
            tag: GHOSTTY_SCROLL_VIEWPORT_DELTA,
            ..Default::default()
        };
        behavior.value.delta = delta;
        unsafe { ghostty_terminal_scroll_viewport(self.terminal, behavior) };
    }

    pub fn scroll_to_bottom(&mut self) {
        let behavior = GhosttyTerminalScrollViewport {
            tag: GHOSTTY_SCROLL_VIEWPORT_BOTTOM,
            ..Default::default()
        };
        unsafe { ghostty_terminal_scroll_viewport(self.terminal, behavior) };
    }

    /// Encodes a key press for the program, honouring the terminal's current
    /// modes (application cursor keys, Kitty keyboard protocol, ...).
    pub fn encode_key(&mut self, input: KeyInput) -> Vec<u8> {
        unsafe {
            ghostty_key_encoder_setopt_from_terminal(self.encoder, self.terminal);
            ghostty_key_event_set_action(self.event, GHOSTTY_KEY_ACTION_PRESS);
            ghostty_key_event_set_key(self.event, input.key);
            ghostty_key_event_set_mods(self.event, input.mods);
            ghostty_key_event_set_consumed_mods(self.event, input.consumed_mods);
            ghostty_key_event_set_composing(self.event, false);
            let text = input.text.unwrap_or("");
            ghostty_key_event_set_utf8(self.event, text.as_ptr().cast(), text.len());
            ghostty_key_event_set_unshifted_codepoint(
                self.event,
                input.unshifted.map_or(0, u32::from),
            );
        }

        let mut buf = [0u8; 128];
        let mut len = 0;
        let result = unsafe {
            ghostty_key_encoder_encode(
                self.encoder,
                self.event,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut len,
            )
        };
        if result == GHOSTTY_SUCCESS {
            buf[..len].to_vec()
        } else {
            Vec::new()
        }
    }

    /// Captures everything needed to draw the viewport.
    pub fn frame(&mut self) -> Frame {
        check!(ghostty_render_state_update(self.render, self.terminal));

        let mut colors = sized!(GhosttyRenderStateColors);
        check!(ghostty_render_state_get(
            self.render,
            GHOSTTY_RENDER_STATE_DATA_COLORS,
            (&mut colors as *mut GhosttyRenderStateColors).cast()
        ));
        let mut cursor = sized!(GhosttyRenderStateCursor);
        check!(ghostty_render_state_get(
            self.render,
            GHOSTTY_RENDER_STATE_DATA_CURSOR,
            (&mut cursor as *mut GhosttyRenderStateCursor).cast()
        ));
        check!(ghostty_render_state_get(
            self.render,
            GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
            (&mut self.row_iter as *mut GhosttyRenderStateRowIterator).cast()
        ));

        let mut rows = Vec::new();
        while unsafe { ghostty_render_state_row_iterator_next(self.row_iter) } {
            check!(ghostty_render_state_row_get(
                self.row_iter,
                GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                (&mut self.cells as *mut GhosttyRenderStateRowCells).cast()
            ));
            let mut row = Vec::new();
            while unsafe { ghostty_render_state_row_cells_next(self.cells) } {
                row.push(self.cell(&colors));
            }
            rows.push(row);
        }
        check!(ghostty_render_state_clean(self.render));

        Frame {
            background: colors.background,
            cursor: (cursor.visible && cursor.viewport_has_value)
                .then_some((cursor.viewport_x, cursor.viewport_y)),
            cursor_color: if colors.cursor_has_value {
                colors.cursor
            } else {
                colors.foreground
            },
            rows,
        }
    }

    fn cell(&self, colors: &GhosttyRenderStateColors) -> Cell {
        let mut len: u32 = 0;
        check!(ghostty_render_state_row_cells_get(
            self.cells,
            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
            (&mut len as *mut u32).cast()
        ));
        let text = if len == 0 {
            String::new()
        } else {
            let mut codepoints = vec![0u32; len as usize];
            check!(ghostty_render_state_row_cells_get(
                self.cells,
                GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                codepoints.as_mut_ptr().cast()
            ));
            codepoints.into_iter().filter_map(char::from_u32).collect()
        };

        let mut style = sized!(GhosttyStyle);
        check!(ghostty_render_state_row_cells_get(
            self.cells,
            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
            (&mut style as *mut GhosttyStyle).cast()
        ));

        let mut fg = resolve(style.fg_color, colors).unwrap_or(colors.foreground);
        let mut bg = resolve(style.bg_color, colors);
        if style.inverse {
            let old_fg = fg;
            fg = bg.unwrap_or(colors.background);
            bg = Some(old_fg);
        }
        Cell {
            text: if style.invisible { String::new() } else { text },
            fg,
            bg,
            bold: style.bold,
            italic: style.italic,
            faint: style.faint,
            underline: style.underline != GHOSTTY_SGR_UNDERLINE_NONE as i32,
            strikethrough: style.strikethrough,
        }
    }
}

fn resolve(color: GhosttyStyleColor, colors: &GhosttyRenderStateColors) -> Option<Rgb> {
    // SAFETY: the tag says which union member is set.
    unsafe {
        match color.tag {
            GHOSTTY_STYLE_COLOR_RGB => Some(color.value.rgb),
            GHOSTTY_STYLE_COLOR_PALETTE => Some(colors.palette[color.value.palette as usize]),
            _ => None,
        }
    }
}

impl Drop for Vt {
    fn drop(&mut self) {
        unsafe {
            ghostty_key_event_free(self.event);
            ghostty_key_encoder_free(self.encoder);
            ghostty_render_state_row_cells_free(self.cells);
            ghostty_render_state_row_iterator_free(self.row_iter);
            ghostty_render_state_free(self.render);
            ghostty_terminal_free(self.terminal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn text(frame: &Frame, row: usize) -> String {
        frame.rows[row]
            .iter()
            .map(|c| c.text.as_str())
            .collect::<String>()
    }

    #[test]
    fn renders_text_and_styles() {
        let mut vt = Vt::new(20, 3, |_| {});
        vt.write(b"hi \x1b[1;31mred\x1b[0m\r\nnext");
        let frame = vt.frame();
        assert_eq!(frame.rows.len(), 3);
        assert_eq!(frame.rows[0].len(), 20);
        assert!(text(&frame, 0).starts_with("hi red"));
        assert!(text(&frame, 1).starts_with("next"));
        let r = &frame.rows[0][3];
        assert!(r.bold);
        let rgb = |c: Rgb| (c.r, c.g, c.b);
        assert_ne!(rgb(r.fg), rgb(frame.rows[0][0].fg));
        assert_eq!(frame.cursor, Some((4, 1)));
    }

    #[test]
    fn answers_device_status_reports() {
        let replies = Rc::new(RefCell::new(Vec::new()));
        let sink = replies.clone();
        let mut vt = Vt::new(80, 24, move |bytes| {
            sink.borrow_mut().extend_from_slice(bytes)
        });
        vt.write(b"\x1b[6n");
        assert_eq!(&replies.borrow()[..], b"\x1b[1;1R");
    }

    #[test]
    fn encodes_keys() {
        let mut vt = Vt::new(80, 24, |_| {});
        let ctrl_c = KeyInput {
            key: GHOSTTY_KEY_C,
            mods: GHOSTTY_MODS_CTRL as GhosttyMods,
            consumed_mods: 0,
            text: Some("c"),
            unshifted: Some('c'),
        };
        assert_eq!(vt.encode_key(ctrl_c), b"\x03");

        let up = KeyInput {
            key: GHOSTTY_KEY_ARROW_UP,
            mods: 0,
            consumed_mods: 0,
            text: None,
            unshifted: None,
        };
        assert_eq!(vt.encode_key(up), b"\x1b[A");
        // Application cursor mode switches arrows to SS3.
        vt.write(b"\x1b[?1h");
        assert_eq!(vt.encode_key(up), b"\x1bOA");
    }

    #[test]
    fn resize_reflows() {
        let mut vt = Vt::new(10, 2, |_| {});
        vt.write(b"0123456789abc");
        vt.resize(20, 2, 8, 16);
        let frame = vt.frame();
        assert_eq!(frame.rows[0].len(), 20);
        assert!(text(&frame, 0).starts_with("0123456789abc"));
    }
}
