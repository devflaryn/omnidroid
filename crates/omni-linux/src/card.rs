//! **A card shown in a display's window instead of the device's frames**: what happened to the app
//! an instance ran -- it crashed, it stopped, it never started -- drawn on the host, so the window
//! never shows Android's own screens in its place (Settings' "Phone is starting..." home). A host
//! program sets one through the control channel (`#host display card <id> <hex>`,
//! [`crate::displays`]); the composer then leaves that display's frames out of its window until the
//! card is taken away (`display show <id>`) or the display is removed.
//!
//! # What it looks like
//!
//! Opaque, the display's size: a near-black ground, a thin red bar along the top, the title in
//! white at twice the body's size, then the lines in light grey -- a line that starts with
//! whitespace (a stack frame, a register dump) dimmer. Lines are word-wrapped to the width -- at a
//! space, or after a `.`, `/`, `$`, ... where a frame's class name gives none ([`wrap`]) -- and
//! the row that would cross the bottom margin is replaced by `...`, so a long trace says it goes on
//! rather than ending mid-way. **The footer stays**: the lines after the card's last empty line
//! (the pool's: where the whole log is, who closes the window) are drawn below that `...`, the
//! rows before it giving way -- a card cut from its end lost them first, and they are what the
//! person can act on (2026-10-19). The margins are 4% of the width (at least 12 px), so a 320x320
//! display still lays out. A title wraps like any line, and is set at the body's size (white
//! still) on a display too narrow for its longest word at its own rather than broken inside the
//! word (`talkingtomca` / `mp`).
//!
//! **The restart block** ([`Restart`], when the card offers one): under the detail lines -- the
//! lines before the last empty one, the crash's log -- after an empty row, and above the footer,
//! centred as a unit. A restart icon (a ring three quarters closed, an arrowhead at its open end:
//! drawn from its shape, [`draw_icon`], no asset) and the headline in white on its first row, the
//! link under it in a soft blue, underlined; with no headline, the icon and the link on one row.
//! Text too wide for a row wraps, each row centred. The block and the footer always stay: on a
//! card too full for everything the detail rows give way first (their last kept one `...`), then
//! the footer, when it would leave the details no row. [`restart_rect`] is where a click lands on
//! the block, from the same [`layout`] the renderer draws, so the two never disagree.
//!
//! **Sizes are pixels, not a fraction of the display**: a display is at most 1600x900's pixels
//! ([`crate::display_window::DISPLAY_MAX_PIXELS`]) and the window scales it to whatever size the
//! person makes it, so a fixed size reads the same in a small window and a full-screen one; a
//! fraction would shrink the text with the window's shape instead.
//!
//! # The font
//!
//! Droid Sans Mono -- the Android image's own `/system/fonts/DroidSansMono.ttf` (Digitized data
//! copyright 2007, Google Corporation; Apache License 2.0) -- rasterized once at build time by
//! FreeType (Pillow) for ASCII 32..126 at two sizes, 20 px in a 12x24 cell and 40 px in a 24x48
//! cell, kept as 4-bit coverage cropped to each glyph's pixels: 28 KiB, in the binary
//! (`device/card/font.bin`; `device/src/card/gen_font.py` makes it and says its format,
//! `device/src/card/build.txt` how it was run). A character outside ASCII shows as `?`, a tab as
//! spaces to the next multiple of four. Coverage is blended linearly onto the ground -- the text is
//! light on dark at fixed colours, where gamma correction would change little.
//!
//! **Fast enough to run on every resize** (the composer re-renders the card at the display's new
//! size): the ground is one row copied down the frame, and only covered glyph pixels are touched
//! -- 0.5 to 1 ms for a 1920x1080 card filled with text, `--release` on an M4 (2026-10-19).
use std::sync::OnceLock;

/// What a card says: a title, then lines of detail (each may be long: the renderer wraps them),
/// and the restart block under them, if it offers one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Card {
    pub title: String,
    pub lines: Vec<String>,
    /// The block drawn centred under the detail lines, above the footer: a restart icon, the
    /// headline, the link the person clicks ([`restart_rect`]).
    pub restart: Option<Restart>,
}

/// A card's restart block: what its headline says (`Restarting in 5 seconds`; empty: none, the
/// link alone) and its link's text (`Click here to restart now`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Restart {
    pub headline: String,
    pub link: String,
}

/// How a card's text gives its restart block: a line `\u{1}restart\t<headline>\t<link>`, taken out
/// of its lines wherever it is. A control character first, so no line of a log is ever taken for
/// it; the pool writes it literally (`omni-pool` does not depend on this crate).
pub const RESTART_LINE: &str = "\u{1}restart\t";

impl Card {
    /// A card from text: its first line the title, every other line one of its lines -- but a
    /// [`RESTART_LINE`], which is its restart block (the last one, should there be more).
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut lines = text.lines().map(str::to_string);
        let title = lines.next().unwrap_or_default();
        let (mut kept, mut restart) = (Vec::new(), None);
        for line in lines {
            match line.strip_prefix(RESTART_LINE) {
                Some(rest) => {
                    let (headline, link) = rest.split_once('\t').unwrap_or(("", rest));
                    restart = Some(Restart { headline: headline.to_string(), link: link.to_string() });
                }
                None => kept.push(line),
            }
        }
        Self { title, lines: kept, restart }
    }
}

/// Where the restart block of `card` drawn at `width` x `height` is: `(x, y, w, h)` in display
/// pixels, the icon, headline and link padded a little so the click is easy to land -- from the
/// layout [`render`] draws. `None`: the card offers no restart.
#[must_use]
pub fn restart_rect(card: &Card, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
    if card.restart.is_none() || width == 0 || height == 0 {
        return None;
    }
    let (x, y, w, h) = layout(card, font(), width, height).block?.bounds;
    let pad = i64::from(HIT_PAD);
    let (left, top) = ((x - pad).max(0), (y - pad).max(0));
    let (right, bottom) = ((x + w + pad).min(i64::from(width)), (y + h + pad).min(i64::from(height)));
    let part = |n: i64| u32::try_from(n).unwrap_or(0);
    (right > left && bottom > top).then(|| (part(left), part(top), part(right - left), part(bottom - top)))
}

/// The text of a `#host display card` command's payload: lowercase hex of the UTF-8 text (one word
/// on the control channel's line, whatever the text holds). `None`: not hex, or not UTF-8.
#[must_use]
pub fn from_hex(hex: &str) -> Option<String> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()).collect();
    String::from_utf8(bytes?).ok()
}

/// The font's two faces (see the module header; the format is `device/src/card/gen_font.py`'s).
const FONT: &[u8] = include_bytes!("../device/card/font.bin");

/// The ground.
const BACKGROUND: [u8; 3] = [16, 16, 18];
/// The bar along the top.
const ACCENT: [u8; 3] = [229, 72, 77];
/// The title.
const TITLE: [u8; 3] = [250, 250, 250];
/// A line.
const BODY: [u8; 3] = [206, 206, 212];
/// A line that starts with whitespace: a stack frame, a register dump.
const DIM: [u8; 3] = [136, 136, 146];
/// The restart block's link: a soft blue, which reads as a link on the near-black ground without
/// glaring beside the grey log.
const LINK: [u8; 3] = [122, 170, 255];

/// The restart icon's side in pixels: a little more than the body's cell (24 px), so the ring and
/// its arrowhead read at a glance beside 20 px text.
const ICON: u32 = 28;
/// Between the restart icon and its row's text.
const ICON_GAP: u32 = 10;
/// Between the headline's last row and the link's first.
const LINK_GAP: u32 = 6;
/// The link's underline: its thickness, and how far under the baseline it starts.
const UNDERLINE: (u32, u32) = (2, 2);
/// Around the restart block's icon and text in [`restart_rect`]: a click just beside a word still
/// lands.
const HIT_PAD: u32 = 8;

/// What a row that would cross the bottom margin becomes.
const ELLIPSIS: &[u8] = b"...";
/// The characters a word longer than a row may break after (besides breaking at a space).
const BREAK_AFTER: &[u8] = b"./\\$:;,()[]{}-_@=&?";

/// One glyph of a face: where its bitmap sits in the cell, its size, and where its coverage starts
/// in [`Face::alpha`].
#[derive(Debug, Clone, Copy, Default)]
struct Glyph {
    x: i64,
    y: i64,
    w: usize,
    h: usize,
    at: usize,
}

/// One size of the font: the cell every glyph advances by, and each glyph's coverage as 0..=255
/// (the file's 4 bits unpacked once, so drawing is a byte a pixel).
#[derive(Debug)]
struct Face {
    cell_w: u32,
    cell_h: u32,
    first: u8,
    glyphs: Vec<Glyph>,
    alpha: Vec<u8>,
}

impl Face {
    /// The glyph `ch` is drawn with: its own, or `?`'s when the face has none.
    fn glyph(&self, ch: u8) -> Glyph {
        let index = |c: u8| usize::from(c.wrapping_sub(self.first));
        self.glyphs.get(index(ch)).or_else(|| self.glyphs.get(index(b'?'))).copied().unwrap_or_default()
    }
}

/// The faces in [`FONT`]: body, then title. `None`: not the format `gen_font.py` writes.
fn read_font(bytes: &[u8]) -> Option<Vec<Face>> {
    fn take<'a>(rest: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, tail) = rest.split_at_checked(n)?;
        *rest = tail;
        Some(head)
    }
    let mut rest = bytes.strip_prefix(b"OCF1")?;
    let count = take(&mut rest, 1)?[0];
    let mut faces = Vec::new();
    for _ in 0..count {
        let &[cell_w, cell_h, first, n] = take(&mut rest, 4)? else { return None };
        let records = take(&mut rest, usize::from(n) * 4)?;
        let len = u32::from_le_bytes(take(&mut rest, 4)?.try_into().ok()?) as usize;
        let mut packed = take(&mut rest, len)?;
        let (mut glyphs, mut alpha) = (Vec::new(), Vec::new());
        for r in records.as_chunks::<4>().0 {
            let (w, h) = (usize::from(r[2]), usize::from(r[3]));
            let bytes = take(&mut packed, (w * h).div_ceil(2))?;
            glyphs.push(Glyph { x: i64::from(r[0] as i8), y: i64::from(r[1] as i8), w, h, at: alpha.len() });
            alpha.extend((0..w * h).map(|i| (if i % 2 == 0 { bytes[i / 2] >> 4 } else { bytes[i / 2] & 15 }) * 17));
        }
        faces.push(Face { cell_w: u32::from(cell_w), cell_h: u32::from(cell_h), first, glyphs, alpha });
    }
    (faces.len() == 2).then_some(faces)
}

/// The font, read once.
fn font() -> &'static [Face] {
    static FACES: OnceLock<Vec<Face>> = OnceLock::new();
    FACES.get_or_init(|| read_font(FONT).expect("device/card/font.bin is gen_font.py's format"))
}

/// A line as the font can draw it: ASCII only (anything else `?`), tabs as spaces to the next
/// multiple of four, no trailing whitespace.
fn printable(line: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    for ch in line.chars() {
        match ch {
            '\t' => out.resize((out.len() / 4 + 1) * 4, b' '),
            ' '..='~' => out.push(ch as u8),
            _ => out.push(b'?'),
        }
    }
    out.truncate(out.trim_ascii_end().len());
    out
}

/// `text` (printable) as rows of at most `cols` characters, each broken at the last space in reach
/// -- or, when that would leave the row less than half full, after the last [`BREAK_AFTER`]
/// character past its first third (`at android.app.` / `ActivityThread.`, not `at` /
/// `android.app.Act`: a frame's only space is after `at`) -- else at a space anywhere, else where
/// the row ends. An indented line's further rows are indented two more than it (at most a third of
/// a row), so a wrapped frame still reads as one. An empty line is one empty row.
fn wrap(text: &[u8], cols: usize) -> Vec<Vec<u8>> {
    let cols = cols.max(1);
    let indent = text.iter().take_while(|&&b| b == b' ').count();
    let hang = if indent > 0 { (indent + 2).min(cols / 3) } else { 0 };
    let mut rows = Vec::new();
    let (mut rest, mut pad) = (text, 0);
    loop {
        let room = cols - pad;
        let row = |part: &[u8]| [&vec![b' '; pad][..], part].concat();
        if rest.len() <= room {
            rows.push(row(rest));
            return rows;
        }
        let lead = rest.iter().take_while(|&&b| b == b' ').count();
        let half = room.div_ceil(2);
        let space = rest[..=room].iter().rposition(|&b| b == b' ').filter(|&i| i > lead);
        let mark = rest[..room].iter().rposition(|b| BREAK_AFTER.contains(b)).map(|i| i + 1).filter(|&i| i >= room.div_ceil(3));
        let cut = match (space, mark) {
            (Some(s), _) if s >= half => s,
            (_, Some(m)) => m,
            (Some(s), None) => s,
            (None, None) => room,
        };
        rows.push(row(rest[..cut].trim_ascii_end()));
        rest = rest[cut..].trim_ascii_start();
        pad = hang;
        if rest.is_empty() {
            return rows;
        }
    }
}

/// The longest run of `text` [`wrap`] has no place to break inside: a word, or a part of one
/// between [`BREAK_AFTER`] characters.
fn longest_word(text: &[u8]) -> usize {
    text.split_inclusive(|&b| b == b' ' || BREAK_AFTER.contains(&b)).map(|w| w.trim_ascii_end().len()).max().unwrap_or(0)
}

/// A row of text placed on the card: its face (0 body, 1 title), colour, characters, where its
/// first cell starts, and whether it is underlined (the restart block's link).
#[derive(Debug)]
struct Row {
    face: usize,
    color: [u8; 3],
    text: Vec<u8>,
    x: i64,
    y: i64,
    underline: bool,
}

impl Row {
    /// A row of the body's face, not underlined: a line of detail, the footer, a `...`.
    fn body(color: [u8; 3], text: Vec<u8>, x: i64, y: i64) -> Self {
        Self { face: 0, color, text, x, y, underline: false }
    }
}

/// The restart block placed: its rows of text, its icon's top-left (an [`ICON`]-sided box), and
/// the bounds of both, `(x, y, w, h)`.
#[derive(Debug)]
struct Block {
    rows: Vec<Row>,
    icon: (i64, i64),
    bounds: (i64, i64, i64, i64),
}

impl Block {
    /// `restart` laid out with its top at 0, every row centred on `width` (see the module
    /// header): at most `text_w` wide, wrapped when its text is wider.
    fn new(restart: &Restart, body: &Face, width: u32, text_w: u32) -> Self {
        let (cell_w, cell_h) = (i64::from(body.cell_w), i64::from(body.cell_h));
        let centred = |w: i64| (i64::from(width) - w) / 2;
        let (headline, link) = (printable(restart.headline.trim()), printable(restart.link.trim()));
        // The icon's row holds the headline's first row, or the link's when there is no headline.
        let (lead, lead_color) = if headline.is_empty() { (&link, LINK) } else { (&headline, TITLE) };
        let cols = (text_w / body.cell_w) as usize;
        let first = wrap(lead, (text_w.saturating_sub(ICON + ICON_GAP) / body.cell_w) as usize).swap_remove(0);
        // The icon centred on the text's capitals (baseline 19, capitals 14 px tall in the 12x24
        // cell): it reaches above the cell by what the box is taller than that middle allows.
        let h = body.glyph(b'H');
        let middle = h.y + h.h as i64 / 2;
        let half = i64::from(ICON) / 2;
        let text_y = (half - middle).max(0);
        let unit = i64::from(ICON + ICON_GAP) + first.len() as i64 * cell_w;
        let icon = (centred(unit).max(0), text_y + middle - half);
        let underline = lead_color == LINK;
        let rest = lead[first.len()..].trim_ascii_start();
        let mut rows = vec![Row { face: 0, color: lead_color, text: first, x: icon.0 + i64::from(ICON + ICON_GAP), y: text_y, underline }];
        let mut y = (text_y + cell_h).max(icon.1 + i64::from(ICON));
        let mut more = |text: &[u8], color, y: &mut i64| {
            // Rows as even as their count allows, since each is centred: `Click here to` /
            // `restart now` on a 320-wide card, not `Click here to restart` / `now`.
            let count = wrap(text, cols).len();
            let even = if count > 1 { (1..=cols).rev().take_while(|&c| wrap(text, c).len() <= count).last().unwrap_or(cols) } else { cols };
            for text in wrap(text, even).into_iter().filter(|t| !t.is_empty()) {
                let x = centred(text.len() as i64 * cell_w).max(0);
                rows.push(Row { face: 0, color, text, x, y: *y, underline: color == LINK });
                *y += cell_h;
            }
        };
        more(rest, lead_color, &mut y);
        if !headline.is_empty() && !link.is_empty() {
            y += i64::from(LINK_GAP);
            more(&link, LINK, &mut y);
        }
        let left = rows.iter().map(|r| r.x).chain([icon.0]).min().unwrap_or(0);
        let right = rows.iter().map(|r| r.x + r.text.len() as i64 * cell_w).chain([icon.0 + i64::from(ICON)]).max().unwrap_or(0);
        Self { rows, icon, bounds: (left, 0, right - left, y) }
    }

    /// The block moved down by `dy`.
    fn moved(mut self, dy: i64) -> Self {
        for row in &mut self.rows {
            row.y += dy;
        }
        self.icon.1 += dy;
        self.bounds.1 += dy;
        self
    }
}

/// Where everything goes on a card of some size.
#[derive(Debug)]
struct Layout {
    /// The bar's height.
    bar: u32,
    rows: Vec<Row>,
    /// The restart block, when the card offers one.
    block: Option<Block>,
}

/// `card` laid out at `width` x `height` (see the module header).
fn layout(card: &Card, faces: &[Face], width: u32, height: u32) -> Layout {
    let (body, title) = (&faces[0], &faces[1]);
    let margin = (width * 4 / 100).max(12);
    let margin_y = margin.min(height * 6 / 100).max(12);
    let bar = (width.min(height) / 180).clamp(3, 6);
    let text_w = width.saturating_sub(2 * margin);
    let (left, cell_h) = (i64::from(margin), i64::from(body.cell_h));
    let bottom = i64::from(height) - i64::from(margin_y);
    let mut rows = Vec::new();
    let mut y = i64::from(bar + margin_y);
    let heading = printable(&card.title);
    let face = usize::from(longest_word(&heading) <= (text_w / title.cell_w) as usize);
    for text in wrap(&heading, (text_w / faces[face].cell_w) as usize) {
        rows.push(Row { face, color: TITLE, text, x: left, y, underline: false });
        y += i64::from(faces[face].cell_h);
    }
    y += i64::from(body.cell_h * 3 / 4);
    let cols = (text_w / body.cell_w) as usize;
    let rows_of = |line: &String| {
        let line = printable(line);
        let color = if line.first() == Some(&b' ') { DIM } else { BODY };
        wrap(&line, cols).into_iter().map(move |text| (color, text))
    };
    // The rows of `lines` up to one past `room`, each with its colour; those past it are not
    // wrapped.
    let rows_within = |lines: &[String], room: usize| {
        let mut out: Vec<([u8; 3], Vec<u8>)> = Vec::new();
        for line in lines {
            if out.len() > room {
                break; // past the bottom already: the rest is not drawn
            }
            out.extend(rows_of(line));
        }
        out
    };
    let mut block = None;
    if let Some(restart) = &card.restart {
        // The detail lines, then (after an empty row) the block, then (after the card's own empty
        // line) the footer: the block and the footer stay, and the details give way.
        let at = card.lines.iter().rposition(|l| l.trim().is_empty());
        let (details, footer) = at.map_or((&card.lines[..], &[][..]), |at| (&card.lines[..at], &card.lines[at + 1..]));
        let placed = Block::new(restart, body, width, text_w);
        let room = usize::try_from((bottom - y - cell_h - placed.bounds.3).max(0) / cell_h).unwrap_or(0);
        let foot: Vec<_> = footer.iter().flat_map(rows_of).collect();
        let mut detail_rows = rows_within(details, room);
        // The footer, after its empty row, when it leaves the details a row (their `...` at least).
        let keep_foot = !foot.is_empty() && foot.len() + 1 + usize::from(!detail_rows.is_empty()) <= room;
        let detail_room = room - if keep_foot { foot.len() + 1 } else { 0 };
        if detail_rows.len() > detail_room {
            detail_rows.truncate(detail_room.saturating_sub(1));
            if detail_room > 0 {
                detail_rows.push((BODY, ELLIPSIS.to_vec()));
            }
        }
        for (color, text) in detail_rows {
            rows.push(Row::body(color, text, left, y));
            y += cell_h;
        }
        let placed = placed.moved(y + cell_h);
        y = placed.bounds.1 + placed.bounds.3;
        block = Some(placed);
        if keep_foot {
            y += cell_h;
            for (color, text) in foot {
                rows.push(Row::body(color, text, left, y));
                y += cell_h;
            }
        }
    } else {
        let room = usize::try_from((bottom - y).max(0) / cell_h).unwrap_or(0);
        let mut body_rows = rows_within(&card.lines, room);
        if body_rows.len() > room {
            // The footer -- the lines after the last empty one, when they take half the room at
            // most -- kept below the `...` that takes the place of the rows that do not fit.
            let foot: Vec<_> = card.lines.iter().rposition(|l| l.trim().is_empty()).map(|at| card.lines[at + 1..].iter().flat_map(rows_of).take(room / 2 + 1).collect()).unwrap_or_default();
            let foot = if foot.len() * 2 <= room { foot } else { Vec::new() };
            body_rows.truncate(room.saturating_sub(1 + foot.len()));
            body_rows.push((BODY, ELLIPSIS.to_vec()));
            body_rows.extend(foot);
        }
        for (color, text) in body_rows {
            rows.push(Row::body(color, text, left, y));
            y += cell_h;
        }
    }
    // A title that crosses the bottom margin itself, or leaves no room for a row (a display too
    // small for it).
    if let Some(over) = rows.iter().position(|r| r.y + i64::from(faces[r.face].cell_h) > bottom) {
        // The rows from `over` on are left out; `...` takes the place of the first of them, or of
        // an earlier one when it would cross the margin itself.
        let mut keep = over;
        while keep > 0 && rows[keep].y + cell_h > bottom {
            keep -= 1;
        }
        let y = rows[keep].y;
        rows.truncate(keep);
        rows.push(Row::body(BODY, ELLIPSIS.to_vec(), left, y));
    }
    Layout { bar, rows, block }
}

/// Blend `glyph` of `face` in `color` onto `out` (`width` x `height` RGBA) with its cell's top-left
/// at `(x, y)`; what falls outside the frame is left out.
fn draw_glyph(out: &mut [u8], (width, height): (u32, u32), face: &Face, glyph: Glyph, (x, y): (i64, i64), color: [u8; 3]) {
    let (gx, gy) = (x + glyph.x, y + glyph.y);
    let cols = gx.max(0)..(gx + glyph.w as i64).min(i64::from(width));
    for py in gy.max(0)..(gy + glyph.h as i64).min(i64::from(height)) {
        let src = glyph.at + (py - gy) as usize * glyph.w;
        for px in cols.clone() {
            let a = u32::from(face.alpha[src + (px - gx) as usize]);
            if a == 0 {
                continue;
            }
            let at = (py as usize * width as usize + px as usize) * 4;
            for (dst, &c) in out[at..at + 3].iter_mut().zip(&color) {
                *dst = ((u32::from(*dst) * (255 - a) + u32::from(c) * a + 127) / 255) as u8;
            }
        }
    }
}

/// Blend `color` at coverage `a` (0..=255) onto the pixel `(x, y)` of `out` (`width` x `height`
/// RGBA); a pixel outside the frame is left out.
fn blend(out: &mut [u8], (width, height): (u32, u32), (x, y): (i64, i64), color: [u8; 3], a: u32) {
    if a == 0 || x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) {
        return;
    }
    let at = (y as usize * width as usize + x as usize) * 4;
    for (dst, &c) in out[at..at + 3].iter_mut().zip(&color) {
        *dst = ((u32::from(*dst) * (255 - a) + u32::from(c) * a + 127) / 255) as u8;
    }
}

/// **The restart icon** in `color`, its [`ICON`]-sided box's top-left at `(x, y)`: a ring three
/// quarters closed, travelling clockwise from just below three o'clock round to half past one,
/// where an arrowhead points on round -- the reload arrow people know from browsers. Drawn from
/// that shape at 4x4 samples a pixel, so the curve and the arrowhead's edges are smooth at 28 px;
/// what falls outside the frame is left out.
fn draw_icon(out: &mut [u8], size: (u32, u32), (x, y): (i64, i64), color: [u8; 3]) {
    let side = f64::from(ICON);
    let centre = side / 2.0;
    let stroke = side * 0.105;
    // The arrowhead: half its base across the ring, and its length along it.
    let (half_base, length) = (stroke * 1.65, stroke * 1.9);
    let radius = centre - half_base - 0.5;
    // Screen angles (y down: clockwise), in radians: the tail, and the arrowhead's base.
    let (tail, head) = (20f64.to_radians(), 315f64.to_radians());
    let span = head - tail;
    let point = |a: f64| (centre + radius * a.cos(), centre + radius * a.sin());
    let (hx, hy) = point(head);
    let (nx, ny) = (head.cos(), head.sin());
    let corners = [(hx + nx * half_base, hy + ny * half_base), (hx - nx * half_base, hy - ny * half_base), (hx - ny * length, hy + nx * length)];
    let (tx, ty) = point(tail);
    let inside = |px: f64, py: f64| {
        let (dx, dy) = (px - centre, py - centre);
        let on_ring = ((dx * dx + dy * dy).sqrt() - radius).abs() <= stroke / 2.0 && (dy.atan2(dx) - tail).rem_euclid(std::f64::consts::TAU) <= span;
        let cap = (px - tx).powi(2) + (py - ty).powi(2) <= (stroke / 2.0).powi(2);
        let side_of = |(ax, ay): (f64, f64), (bx, by): (f64, f64)| (bx - ax) * (py - ay) - (by - ay) * (px - ax);
        let s = [side_of(corners[0], corners[1]), side_of(corners[1], corners[2]), side_of(corners[2], corners[0])];
        on_ring || cap || s.iter().all(|&v| v >= 0.0) || s.iter().all(|&v| v <= 0.0)
    };
    for py in 0..ICON {
        for px in 0..ICON {
            let samples = (0..16).filter(|i| inside(f64::from(px) + (f64::from(i % 4) + 0.5) / 4.0, f64::from(py) + (f64::from(i / 4) + 0.5) / 4.0)).count();
            blend(out, size, (x + i64::from(px), y + i64::from(py)), color, samples as u32 * 255 / 16);
        }
    }
}

/// `card` drawn at `width` x `height`: RGBA, `width * height * 4` bytes, every pixel opaque.
#[must_use]
pub fn render(card: &Card, width: u32, height: u32) -> Vec<u8> {
    let pixel = |[r, g, b]: [u8; 3]| [r, g, b, 255];
    let mut out = pixel(BACKGROUND).repeat(width as usize).repeat(height as usize);
    if out.is_empty() {
        return out;
    }
    let faces = font();
    let layout = layout(card, faces, width, height);
    let bar = pixel(ACCENT).repeat(width as usize);
    for line in out.chunks_exact_mut(bar.len()).take(layout.bar as usize) {
        line.copy_from_slice(&bar);
    }
    let block = layout.block.as_ref();
    for row in layout.rows.iter().chain(block.into_iter().flat_map(|b| &b.rows)) {
        let face = &faces[row.face];
        for (i, &ch) in row.text.iter().enumerate() {
            if ch != b' ' {
                let x = row.x + i as i64 * i64::from(face.cell_w);
                draw_glyph(&mut out, (width, height), face, face.glyph(ch), (x, row.y), row.color);
            }
        }
        if row.underline {
            // Under the baseline (the bottom of `H`), the row's cells wide.
            let h = face.glyph(b'H');
            let top = row.y + h.y + h.h as i64 + i64::from(UNDERLINE.1);
            for (x, y) in (row.x..row.x + row.text.len() as i64 * i64::from(face.cell_w)).flat_map(|x| (top..top + i64::from(UNDERLINE.0)).map(move |y| (x, y))) {
                blend(&mut out, (width, height), (x, y), row.color, 255);
            }
        }
    }
    if let Some(b) = block {
        draw_icon(&mut out, (width, height), b.icon, TITLE);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_is_read_from_its_text_and_its_payload() {
        let card = Card::parse("Talking Tom stopped\nNative crash: SIGSEGV\npc 0x1234");
        assert_eq!(card.title, "Talking Tom stopped");
        assert_eq!(card.lines, vec!["Native crash: SIGSEGV".to_string(), "pc 0x1234".to_string()]);
        assert_eq!(from_hex("4869").as_deref(), Some("Hi"));
        assert_eq!(from_hex("486"), None);
        assert_eq!(from_hex("zz"), None);
        assert_eq!(render(&card, 4, 3).len(), 4 * 3 * 4);
    }

    /// The pool's restart line comes out of the lines wherever it is, the footer after it kept.
    #[test]
    fn a_card_s_restart_line_is_its_block_not_a_line() {
        let text = "com.a crashed\nNative crash: SIGSEGV\n\u{1}restart\tRestarting in 5 seconds\tClick here to restart now\n\nClose this window when you are done.";
        let card = Card::parse(text);
        assert_eq!(card.lines, ["Native crash: SIGSEGV", "", "Close this window when you are done."]);
        assert_eq!(card.restart, Some(Restart { headline: "Restarting in 5 seconds".into(), link: "Click here to restart now".into() }));
        let link_alone = Card::parse("com.a stopped\n\u{1}restart\t\tClick here to restart");
        assert_eq!(link_alone.restart, Some(Restart { headline: String::new(), link: "Click here to restart".into() }), "no headline");
        assert!(link_alone.lines.is_empty());
        assert_eq!(Card::parse("com.a stopped\nrestart\tx\ty").restart, None, "only after its control character");
        let headline_alone = Card::parse("com.a crashed\n\u{1}restart\tRestarting in 2 seconds\t");
        assert_eq!(headline_alone.restart, Some(Restart { headline: "Restarting in 2 seconds".into(), link: String::new() }), "no link");
    }

    /// A card like the pool shows for a crash: a long exception line, indented frames (as logcat
    /// writes them, after a tab).
    fn crash() -> Card {
        let lines = [
            "Java exception: android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash",
            "\tat android.app.ActivityThread.throwRemoteServiceException(ActivityThread.java:2257)",
            "\tat android.app.ActivityThread$H.handleMessage(ActivityThread.java:2541)",
            "",
            "Close this window when you are done.",
        ];
        Card { title: "com.outfit7.talkingtomcamp crashed".to_string(), lines: lines.map(str::to_string).to_vec(), restart: None }
    }

    #[test]
    fn the_font_is_two_monospaced_faces_of_printable_ascii() {
        let faces = read_font(FONT).expect("the font reads");
        assert_eq!((faces[0].cell_w, faces[0].cell_h), (12, 24));
        assert_eq!((faces[1].cell_w, faces[1].cell_h), (24, 48));
        for face in &faces {
            assert_eq!((face.first, face.glyphs.len()), (32, 95));
            assert_eq!(face.glyphs[0].w * face.glyphs[0].h, 0, "a space covers nothing");
            let a = face.glyph(b'A');
            assert!(face.alpha[a.at..a.at + a.w * a.h].contains(&255), "A is drawn, fully covering somewhere");
            let (q, odd) = (face.glyph(b'?'), face.glyph(0x7f));
            assert_eq!((odd.at, odd.w, odd.h), (q.at, q.w, q.h), "what the font lacks is ?");
        }
        assert_eq!(printable("caf\u{e9}\tx  "), b"caf?    x");
    }

    #[test]
    fn a_card_fills_the_frame_opaque_with_its_text_drawn() {
        for (w, h) in [(1280, 720), (400, 700), (320, 320), (1, 1), (0, 5)] {
            let px = render(&crash(), w, h);
            assert_eq!(px.len(), w as usize * h as usize * 4);
            assert!(px.as_chunks::<4>().0.iter().all(|p| p[3] == 255), "{w}x{h}: every pixel opaque");
        }
        let (w, h) = (1280u32, 720u32);
        let px = render(&crash(), w, h);
        let at = |x: u32, y: u32| &px[(y * w + x) as usize * 4..][..3];
        assert_eq!(at(w / 2, 0), ACCENT, "the bar along the top");
        assert_eq!(at(w / 2, h - 1), BACKGROUND);
        let title = &layout(&crash(), font(), w, h).rows[0];
        let band = || (title.y as u32..title.y as u32 + 48).flat_map(|y| (0..w).map(move |x| (x, y)));
        let drawn = band().filter(|&(x, y)| at(x, y) != BACKGROUND).count();
        assert!(drawn > 2000, "the title is drawn: {drawn} pixels differ from the ground");
        assert!(band().any(|(x, y)| at(x, y) == TITLE), "its strokes reach the title's colour");
    }

    #[test]
    fn a_narrow_display_wraps_the_same_card_into_more_rows() {
        let wide = layout(&crash(), font(), 1280, 720);
        let narrow = layout(&crash(), font(), 400, 700);
        assert!(narrow.rows.len() > wide.rows.len() + 3, "{} rows at 400 wide, {} at 1280", narrow.rows.len(), wide.rows.len());
        assert!(narrow.rows.iter().filter(|r| r.face == 1).count() > 1, "the title wraps at 400 wide");
        for r in &narrow.rows {
            let cols = (400 - 2 * 16) / font()[r.face].cell_w;
            assert!(r.text.len() <= cols as usize, "{:?} is wider than {cols}", String::from_utf8_lossy(&r.text));
        }
        let frame: Vec<_> = narrow.rows.iter().filter(|r| r.color == DIM).map(|r| String::from_utf8_lossy(&r.text).into_owned()).collect();
        assert_eq!(frame[0], "    at android.app.", "a frame keeps its indent: {frame:?}");
        assert!(frame[1].starts_with("      ActivityThread."), "and its further rows two more: {frame:?}");
    }

    #[test]
    fn words_wrap_at_spaces_then_at_punctuation_then_anywhere() {
        let rows = |s: &str, cols| wrap(s.as_bytes(), cols).into_iter().map(|r| String::from_utf8(r).unwrap()).collect::<Vec<_>>();
        assert_eq!(rows("Close this window when you are done.", 16), ["Close this", "window when you", "are done."]);
        assert_eq!(rows("com.outfit7.talkingtomcamp crashed", 15), ["com.outfit7.", "talkingtomcamp", "crashed"]);
        assert_eq!(rows("abcdefghijklmnop", 6), ["abcdef", "ghijkl", "mnop"]);
        assert_eq!(rows("  x0 0000 x1 1111", 12), ["  x0 0000 x1", "    1111"]);
        assert_eq!(rows("", 10), [""]);
        assert_eq!(rows("abc", 0), ["a", "b", "c"]);
    }

    #[test]
    fn a_title_too_wide_for_its_size_is_set_at_the_body_s() {
        let small = layout(&crash(), font(), 320, 320);
        let title: Vec<_> = small.rows.iter().take_while(|r| r.color == TITLE).map(|r| (r.face, String::from_utf8_lossy(&r.text).into_owned())).collect();
        assert_eq!(title, [(0, "com.outfit7.".to_string()), (0, "talkingtomcamp crashed".to_string())]);
        assert_eq!(longest_word(b"com.outfit7.talkingtomcamp crashed"), 14);
        assert!(layout(&crash(), font(), 400, 700).rows[0].face == 1, "talkingtomcamp fits at 400 wide");
    }

    #[test]
    fn rows_past_the_bottom_end_in_an_ellipsis() {
        let mut card = crash();
        card.lines.extend((0..200).map(|i| format!("    x{i} 0000007fd5a3c000")));
        for (w, h) in [(1280, 720), (320, 320)] {
            let l = layout(&card, font(), w, h);
            let last = l.rows.last().unwrap();
            assert_eq!(last.text, ELLIPSIS, "{w}x{h}");
            assert!(last.y + 24 <= i64::from(h) - 12, "{w}x{h}: the ellipsis is inside the margin");
            assert!(l.rows.len() < 40);
        }
    }

    /// The pool's footer after a trace too long for the card: kept, below the `...`.
    #[test]
    fn a_long_card_keeps_its_footer_below_the_ellipsis() {
        let mut card = crash();
        card.lines.truncate(3);
        card.lines.extend((0..60).map(|i| format!("\tat com.a.Frame.f{i}(Frame.java:{i})")));
        card.lines.extend(["", "The whole log: /t/omni-warm-1.log", "Close this window when you are done."].map(str::to_string));
        let (w, h) = (1280, 720);
        let l = layout(&card, font(), w, h);
        let text: Vec<String> = l.rows.iter().map(|r| String::from_utf8_lossy(&r.text).into_owned()).collect();
        assert_eq!(text[text.len() - 3..], ["...", "The whole log: /t/omni-warm-1.log", "Close this window when you are done."], "{text:#?}");
        assert!(l.rows.iter().all(|r| r.y + i64::from(font()[r.face].cell_h) <= i64::from(h) - 43), "every row inside the margin");
        assert_eq!(l.rows.len(), 1 + 23, "the title and the room's 23 rows, all used");
        assert!(text.contains(&"    at com.a.Frame.f0(Frame.java:0)".to_string()), "the trace's beginning stays: {text:#?}");
        // A card that fits is drawn whole, its footer where it is.
        card.lines.drain(3..60);
        let text: Vec<String> = layout(&card, font(), w, h).rows.iter().map(|r| String::from_utf8_lossy(&r.text).into_owned()).collect();
        assert!(!text.contains(&"...".to_string()) && text.ends_with(&["".to_string(), "The whole log: /t/omni-warm-1.log".to_string(), "Close this window when you are done.".to_string()]), "{text:#?}");
    }

    /// The pool's text for a crash: the diagnosis' title and details, the restart line after them,
    /// then the empty line and the footer (`omni_pool::pool::card_text`).
    fn crash_with(restart: &str) -> Card {
        let mut text = vec![crash().title];
        text.extend(crash().lines[..3].iter().cloned());
        text.push(format!("{RESTART_LINE}{restart}"));
        text.extend(["", "The whole log: /t/omni-warm-1.log", "Close this window when you are done."].map(str::to_string));
        Card::parse(&text.join("\n"))
    }

    const AUTO: &str = "Automatically restarting in 5 seconds\tClick here to restart now";

    /// The pixel `(x, y)` of a `width`-wide frame.
    fn at(px: &[u8], width: u32, (x, y): (u32, u32)) -> [u8; 3] {
        px[(y * width + x) as usize * 4..][..3].try_into().unwrap()
    }

    /// The block is drawn under the log's last row and above the footer, centred, its clickable
    /// rectangle around what is drawn -- the icon in white, the link in its blue -- and nothing
    /// drawn between the log and it.
    #[test]
    fn the_restart_block_is_drawn_centred_under_the_log_above_the_footer() {
        for (w, h) in [(1280u32, 720u32), (1600, 900), (400, 700)] {
            let card = crash_with(AUTO);
            let l = layout(&card, font(), w, h);
            let b = l.block.as_ref().expect("a block");
            let (rx, ry, rw, rh) = restart_rect(&card, w, h).expect("a rect");
            assert!((i64::from(rx + rw / 2) - i64::from(w / 2)).abs() <= 2, "{w}x{h}: centred: {rx} + {rw}/2");
            let detail_end = l.rows.iter().filter(|r| r.y < b.bounds.1).map(|r| r.y + 24).max().unwrap();
            let footer = l.rows.iter().find(|r| r.text.starts_with(b"The whole log")).expect("the footer");
            assert!(i64::from(ry) >= detail_end + 8, "{w}x{h}: an empty row under the log's last row ({detail_end}), the rect at {ry}");
            assert!(i64::from(ry + rh) <= footer.y, "{w}x{h}: above the footer at {}: {ry} + {rh}", footer.y);
            let px = render(&card, w, h);
            let rect = || (ry..ry + rh).flat_map(|y| (rx..rx + rw).map(move |x| (x, y)));
            let drawn = rect().filter(|&p| at(&px, w, p) != BACKGROUND).count();
            assert!(drawn > 1500, "{w}x{h}: the block is drawn: {drawn} pixels differ from the ground");
            assert!(rect().any(|p| at(&px, w, p) == LINK), "{w}x{h}: the link's underline in its colour");
            let (ix, iy) = (u32::try_from(b.icon.0).unwrap(), u32::try_from(b.icon.1).unwrap());
            let icon = (iy..iy + ICON).flat_map(|y| (ix..ix + ICON).map(move |x| (x, y))).filter(|&p| at(&px, w, p) == TITLE).count();
            assert!(icon > 40, "{w}x{h}: the icon is drawn, {icon} pixels in full");
            let gap = (detail_end as u32..ry).flat_map(|y| (0..w).map(move |x| (x, y)));
            assert!(gap.into_iter().all(|p| at(&px, w, p) == BACKGROUND), "{w}x{h}: the empty row is empty");
        }
        let wide = layout(&crash_with(AUTO), font(), 1280, 720);
        let rows: Vec<_> = wide.block.unwrap().rows.into_iter().map(|r| (String::from_utf8(r.text).unwrap(), r.underline)).collect();
        assert_eq!(rows, [("Automatically restarting in 5 seconds".to_string(), false), ("Click here to restart now".to_string(), true)]);
    }

    /// No headline: the icon and the link on one row.
    #[test]
    fn a_block_without_a_headline_is_the_icon_and_the_link() {
        let card = crash_with("\tClick here to restart");
        let b = layout(&card, font(), 400, 700).block.unwrap();
        assert_eq!(b.rows.len(), 1);
        assert!(b.rows[0].text == b"Click here to restart" && b.rows[0].color == LINK && b.rows[0].underline);
        assert!(b.icon.0 + i64::from(ICON) < b.rows[0].x && b.icon.1 < b.rows[0].y + 24 && b.rows[0].y < b.icon.1 + i64::from(ICON), "beside the icon: {b:?}");
        assert!(restart_rect(&crash(), 1280, 720).is_none() && restart_rect(&card, 0, 700).is_none(), "no block, or no frame: no rect");
        assert!(!render(&crash(), 1280, 720).as_chunks::<4>().0.iter().any(|p| p[..3] == LINK), "and none drawn");
    }

    /// A card stuffed with a trace keeps its block and its footer: the details give way, ending in
    /// `...` above the block; and a 320x320 display still lays it all out inside the frame.
    #[test]
    fn a_full_card_keeps_its_block_and_its_footer() {
        let mut card = crash_with(AUTO);
        card.lines.splice(3..3, (0..60).map(|i| format!("\tat com.a.Frame.f{i}(Frame.java:{i})")));
        for (w, h) in [(1280u32, 720u32), (400, 700), (320, 320)] {
            let l = layout(&card, font(), w, h);
            let text: Vec<String> = l.rows.iter().map(|r| String::from_utf8_lossy(&r.text).into_owned()).collect();
            let b = l.block.as_ref().expect("the block stays");
            let bottom = i64::from(h) - i64::from((w * 4 / 100).max(12).min(h * 6 / 100).max(12));
            assert!(b.bounds.1 + b.bounds.3 <= bottom, "{w}x{h}: the block inside the margin: {:?}", b.bounds);
            assert!(l.rows.iter().all(|r| r.y + i64::from(font()[r.face].cell_h) <= bottom), "{w}x{h}: every row inside the margin");
            let (rx, ry, rw, rh) = restart_rect(&card, w, h).unwrap();
            assert!(rx + rw <= w && ry + rh <= h, "{w}x{h}: the rect inside the frame");
            for r in &b.rows {
                assert!(r.x >= 0 && r.x + r.text.len() as i64 * 12 <= i64::from(w), "{w}x{h}: {:?} inside the frame", String::from_utf8_lossy(&r.text));
            }
            let ellipsis = text.iter().position(|t| t == "...").unwrap_or_else(|| panic!("{w}x{h}: the details end in ...: {text:#?}"));
            assert!(l.rows[ellipsis].y < b.bounds.1, "{w}x{h}: above the block");
            if h >= 700 {
                let footer = text.iter().position(|t| t.starts_with("The whole log:")).unwrap_or_else(|| panic!("{w}x{h}: the footer stays: {text:#?}"));
                assert!(text.last().unwrap().ends_with("done.") && l.rows[footer].y > b.bounds.1 + b.bounds.3, "{w}x{h}: the footer whole, under the block: {text:#?}");
            }
        }
        for (w, h) in [(1, 1), (0, 5), (64, 32)] {
            assert_eq!(render(&card, w, h).len(), w as usize * h as usize * 4, "{w}x{h}");
            let _ = restart_rect(&card, w, h).map(|(x, y, rw, rh)| assert!(x + rw <= w && y + rh <= h));
        }
    }

    /// Writes `omni-card-1280.png`, `omni-card-400.png` and `omni-card-320.png` into the temp
    /// directory, to be looked at, and says how long a 1920x1080 card full of text takes to render.
    #[test]
    #[ignore = "writes PNGs to look at"]
    fn preview() {
        let mut card = crash();
        let more = ["\tat android.os.Handler.dispatchMessage(Handler.java:107)", "\tat android.os.Looper.loopOnce(Looper.java:232)", "\tat android.os.Looper.loop(Looper.java:317)", "\tat android.app.ActivityThread.main(ActivityThread.java:8705)"];
        card.lines.splice(3..3, more.map(str::to_string));
        card.lines.insert(card.lines.len() - 1, "The whole log: /private/tmp/omni-pool/omni-warm-1792431678.log".to_string());
        for (w, h) in [(1280, 720), (400, 700), (320, 320)] {
            let px = render(&card, w, h);
            let fb = crate::hal::framebuffer::Framebuffer::new(w, h);
            fb.present_frame(&px, w, h, w);
            std::fs::write(std::env::temp_dir().join(format!("omni-card-{w}.png")), fb.png()).unwrap();
        }
        card.lines.extend((0..100).map(|i| format!("    x{i} 0000007fd5a3c000  x{} 0000007fd5a3c008  x{} 0000000000000000  sp 0000007fd5a3bf40", i + 1, i + 2)));
        let _ = render(&card, 1920, 1080);
        let t = std::time::Instant::now();
        for _ in 0..10 {
            let _ = render(&card, 1920, 1080);
        }
        eprintln!("1920x1080: {:?} a render", t.elapsed() / 10);
    }

    /// Writes the restart block's cards into the temp directory, to be looked at: `card-restart-`
    /// `auto-1280.png`, `manual-1280.png`, `plain-400.png`, `full-1280.png` and `320.png`.
    #[test]
    #[ignore = "writes PNGs to look at"]
    fn preview_restart() {
        let frames = ["Java exception: java.lang.IllegalStateException: the asset bundle is gone", "\tat com.outfit7.engine.AssetManager.open(AssetManager.java:212)", "\tat com.outfit7.engine.Scene.load(Scene.java:88)", "\tat com.outfit7.talkingtomcamp.Main.onResume(Main.java:301)", "\tat android.app.Activity.performResume(Activity.java:8939)", "\tat android.app.ActivityThread.performResumeActivity(ActivityThread.java:5196)", "\tat android.os.Handler.dispatchMessage(Handler.java:107)", "\tat android.os.Looper.loop(Looper.java:317)", "\tat android.app.ActivityThread.main(ActivityThread.java:8705)"];
        let card = |title: &str, details: &[String], restart: &str| {
            let mut text = vec![title.to_string()];
            text.extend(details.iter().cloned());
            text.push(format!("{RESTART_LINE}{restart}"));
            text.extend(["", "The whole log: /private/tmp/omni-pool/omni-warm-1792431678.log", "Close this window when you are done."].map(str::to_string));
            Card::parse(&text.join("\n"))
        };
        let details: Vec<String> = frames.map(str::to_string).to_vec();
        let mut full = details.clone();
        full.splice(3..3, (0..60).map(|i| format!("\tat com.outfit7.engine.Frame{i}.run(Frame{i}.java:{})", 40 + i)));
        let auto = card("com.outfit7.talkingtomcamp crashed", &details, AUTO);
        let cards = [
            ("auto-1280", auto.clone(), 1280, 720),
            ("manual-1280", card("com.outfit7.talkingtomcamp is restarting", &["Restart asked (restart_instance).".to_string()], "Restarting in 3 seconds\tClick here to restart now"), 1280, 720),
            ("plain-400", card("com.outfit7.talkingtomcamp crashed", &details, "\tClick here to restart"), 400, 700),
            ("full-1280", card("com.outfit7.talkingtomcamp crashed", &full, AUTO), 1280, 720),
            ("320", auto, 320, 320),
        ];
        for (name, card, w, h) in cards {
            let px = render(&card, w, h);
            let fb = crate::hal::framebuffer::Framebuffer::new(w, h);
            fb.present_frame(&px, w, h, w);
            std::fs::write(std::env::temp_dir().join(format!("card-restart-{name}.png")), fb.png()).unwrap();
        }
    }
}
