//! Inline run layout: the word-wrapper, the font cache behind it, and the primitive
//! draws every block type shares.
//!
//! This is the hot loop of the whole renderer - it runs per block, per paint - so the
//! fonts are cached per style rather than created per run, and the tokenizer walks the
//! text once.

use super::*;

/// The five font variants a block draws with, created once and freed together.
pub(super) struct Fonts {
    pub(super) reg: HFONT,
    pub(super) bold: HFONT,
    pub(super) ital: HFONT,
    pub(super) bi: HFONT,
    pub(super) mono: HFONT,
    pub(super) px: i32,
    pub(super) base_bold: bool,
    pub(super) base_italic: bool,
}

impl Fonts {
    pub(super) unsafe fn new(hwnd: HWND, px: i32, base_bold: bool, base_italic: bool) -> Fonts {
        Fonts {
            reg: font(hwnd, px, base_bold, base_italic, false),
            bold: font(hwnd, px, true, base_italic, false),
            ital: font(hwnd, px, base_bold, true, false),
            bi: font(hwnd, px, true, true, false),
            mono: font(hwnd, px - 1, false, false, true),
            px,
            base_bold,
            base_italic,
        }
    }
    pub(super) fn pick(&self, r: &Run) -> HFONT {
        if r.code {
            return self.mono;
        }
        let b = self.base_bold || r.bold;
        let i = self.base_italic || r.italic;
        match (b, i) {
            (true, true) => self.bi,
            (true, false) => self.bold,
            (false, true) => self.ital,
            (false, false) => self.reg,
        }
    }
    /// The spec of the font [`Fonts::pick`] would return — recorded per drawn token so
    /// hit-testing can re-create it after these handles are freed. MUST mirror `pick`/`new`.
    pub(super) fn spec(&self, r: &Run) -> FontSpec {
        if r.code {
            return FontSpec {
                px: self.px - 1,
                bold: false,
                italic: false,
                mono: true,
            };
        }
        FontSpec {
            px: self.px,
            bold: self.base_bold || r.bold,
            italic: self.base_italic || r.italic,
            mono: false,
        }
    }
    pub(super) unsafe fn free(self) {
        for f in [self.reg, self.bold, self.ital, self.bi, self.mono] {
            let _ = DeleteObject(f.into());
        }
    }
}

/// Palette + DPI-scaled constants shared by every `run_block` call of one render pass.
pub(super) struct RunCtx {
    pub(super) code_bg: u32,
    pub(super) accent: u32,
    pub(super) base_color: u32,
    pub(super) code_pad: i32,
    pub(super) line_lead: i32,
    pub(super) ul_off: i32,
}

pub(super) fn ctx_for(hwnd: HWND, c: &MdColors, base_color: u32) -> RunCtx {
    RunCtx {
        code_bg: c.code_bg,
        accent: c.accent,
        base_color,
        code_pad: crate::win::dpi_scale(hwnd, 3),
        line_lead: crate::win::dpi_scale(hwnd, 3),
        ul_off: crate::win::dpi_scale(hwnd, 2),
    }
}

/// A measured, placeable token from the flattened run stream. `doc` is the token's slice of the
/// selection document (`None` on dry/unselectable passes).
pub(super) enum Tok {
    Word {
        s: Vec<u16>,
        w: i32,
        pad: i32,
        font: HFONT,
        color: u32,
        code: bool,
        strike: bool,
        link: Option<String>,
        doc: Option<(usize, usize)>,
        spec: FontSpec,
    },
    Space(i32),
    Break,
}

/// Selection wiring for one [`run_block`] call: the active range, the document (to measure a
/// partially-selected word), this block's per-run document offsets, and the hit collector.
pub(super) struct RunSel<'a> {
    pub(super) range: Option<(usize, usize)>,
    pub(super) doc: &'a str,
    pub(super) bases: &'a [usize],
    pub(super) hits: &'a mut Vec<SelHit>,
    pub(super) bg: u32,
}

/// Word-wrap + draw a block's inline `runs` starting at `(x0, y)` within `width`.
/// `align`: 0 left, 1 center, 2 right (per-line offset). `dry` measures without drawing
/// (no GDI output, no link/selection collection). Returns `(y_after, widest_line)`.
#[allow(clippy::too_many_arguments)] // GDI layout core: hdc + geometry + mode flags, no struct gain
pub(super) unsafe fn run_block(
    hdc: HDC,
    runs: &[Run],
    fonts: &Fonts,
    x0: i32,
    y: i32,
    width: i32,
    align: u8,
    dry: bool,
    ctx: &RunCtx,
    links: &mut Vec<LinkHit>,
    mut sel: Option<&mut RunSel>,
) -> (i32, i32) {
    if runs.iter().all(|r| r.text.trim().is_empty()) {
        return (y, 0);
    }
    // Line height from the regular font's metrics + a little leading.
    let old_font = SelectObject(hdc, fonts.reg.into());
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm);
    let line_h = tm.tmHeight + tm.tmExternalLeading + ctx.line_lead;

    // Flatten runs -> measured tokens (words / spaces / hard breaks), each remembering the run
    // bytes it came from so it maps back to the selection document.
    let mut toks: Vec<Tok> = Vec::new();
    for (ri, r) in runs.iter().enumerate() {
        let f = fonts.pick(r);
        let spec = fonts.spec(r);
        let color = if r.link.is_some() {
            ctx.accent
        } else {
            ctx.base_color
        };
        let pad = if r.code { ctx.code_pad } else { 0 };
        SelectObject(hdc, f.into());
        let base = sel.as_ref().and_then(|s| s.bases.get(ri).copied());
        let mut word: Vec<u16> = Vec::new();
        let mut wstart = 0usize; // byte offset in `r.text` where the pending word began
        macro_rules! flush_word {
            ($wend:expr) => {
                if !word.is_empty() {
                    let mut sz = SIZE::default();
                    let _ = GetTextExtentPoint32W(hdc, &word, &mut sz);
                    toks.push(Tok::Word {
                        s: core::mem::take(&mut word),
                        w: sz.cx + 2 * pad,
                        pad,
                        font: f,
                        color,
                        code: r.code,
                        strike: r.strike,
                        link: r.link.clone(),
                        doc: base.map(|b| (b + wstart, b + $wend)),
                        spec,
                    });
                }
            };
        }
        let mut chars = r.text.char_indices().peekable();
        while let Some((ci, ch)) = chars.next() {
            match ch {
                '\n' => {
                    flush_word!(ci);
                    toks.push(Tok::Break);
                }
                ' ' | '\t' => {
                    flush_word!(ci);
                    let mut sz = SIZE::default();
                    let sp = [b' ' as u16];
                    let _ = GetTextExtentPoint32W(hdc, &sp, &mut sz);
                    toks.push(Tok::Space(sz.cx));
                }
                _ => {
                    if word.is_empty() {
                        wstart = ci;
                    }
                    let mut b = [0u16; 2];
                    for u in ch.encode_utf16(&mut b) {
                        word.push(*u);
                    }
                    // Scripts that don't put spaces between words get their break
                    // opportunities here instead. Without this a Chinese/Japanese paragraph is
                    // ONE token, and the greedy line-breaker below places an over-wide token
                    // anyway — so the whole paragraph ran off the pane edge and was clipped.
                    if let Some(&(ni, next)) = chars.peek() {
                        if can_break_between(ch, next) {
                            flush_word!(ni);
                        }
                    }
                }
            }
        }
        flush_word!(r.text.len());
    }

    // Break into lines (greedy), remembering each placed word's line-relative x.
    let mut lines: Vec<(Vec<(i32, usize)>, i32)> = Vec::new(); // (placements, line width)
    let mut cur: Vec<(i32, usize)> = Vec::new();
    let mut cx = 0;
    let mut pending_space = 0;
    let mut line_start = true;
    for (idx, tok) in toks.iter().enumerate() {
        match tok {
            Tok::Break => {
                lines.push((core::mem::take(&mut cur), cx));
                cx = 0;
                pending_space = 0;
                line_start = true;
            }
            Tok::Space(sw) => {
                if !line_start {
                    pending_space += *sw;
                }
            }
            Tok::Word { w, .. } => {
                if !line_start && cx + pending_space + *w > width {
                    lines.push((core::mem::take(&mut cur), cx));
                    cx = 0;
                    pending_space = 0;
                    line_start = true;
                }
                if !line_start {
                    cx += pending_space;
                }
                pending_space = 0;
                cur.push((cx, idx));
                cx += *w;
                line_start = false;
            }
        }
    }
    if !cur.is_empty() || !line_start {
        lines.push((cur, cx));
    }
    if lines.is_empty() {
        SelectObject(hdc, old_font);
        return (y, 0);
    }
    let max_w = lines.iter().map(|(_, w)| *w).max().unwrap_or(0);

    // Copied out so the draw loop can read the selection while `sel` is mutably reborrowed for
    // the per-line fill.
    let (sel_rng, sel_bg) = match sel.as_ref() {
        Some(s) => (s.range, s.bg),
        None => (None, 0),
    };
    if !dry {
        for (li, (placed, lw)) in lines.iter().enumerate() {
            let xoff = match align {
                1 => (width - lw).max(0) / 2,
                2 => (width - lw).max(0),
                _ => 0,
            };
            let cy = y + li as i32 * line_h;
            // Selection fill + hit rects BEFORE the glyphs — an opaque fill after would erase them.
            if let Some(s) = sel.as_deref_mut() {
                line_sel(hdc, &toks, placed, x0 + xoff, cy, line_h, s);
            }
            for (rx, idx) in placed {
                let Tok::Word {
                    s,
                    w,
                    pad,
                    font,
                    color,
                    code,
                    strike,
                    link,
                    doc,
                    ..
                } = &toks[*idx]
                else {
                    continue;
                };
                let cx = x0 + xoff + rx;
                SelectObject(hdc, (*font).into());
                SetTextColor(hdc, COLORREF(*color));
                if *code {
                    // Shaded panel behind inline code (opaque ExtTextOut). It would paint OVER the
                    // selection fill, so when the span is selected the panel IS the highlight.
                    let hot = sel_rng
                        .zip(*doc)
                        .is_some_and(|((ss, se), (ds, de))| ss < de && se > ds);
                    let r = RECT {
                        left: cx,
                        top: cy,
                        right: cx + *w,
                        bottom: cy + line_h,
                    };
                    SetBkColor(hdc, COLORREF(if hot { sel_bg } else { ctx.code_bg }));
                    SetBkMode(hdc, OPAQUE);
                    let _ = ExtTextOutW(
                        hdc,
                        cx + *pad,
                        cy,
                        ETO_OPAQUE,
                        Some(&r as *const RECT),
                        PCWSTR(s.as_ptr()),
                        s.len() as u32,
                        None,
                    );
                    SetBkMode(hdc, TRANSPARENT);
                } else {
                    let _ = ExtTextOutW(
                        hdc,
                        cx,
                        cy,
                        ETO_OPTIONS(0),
                        None,
                        PCWSTR(s.as_ptr()),
                        s.len() as u32,
                        None,
                    );
                }
                if *strike {
                    hline(hdc, cx + *pad, cx + *w - *pad, cy + line_h / 2, *color);
                }
                if let Some(url) = link {
                    hline(
                        hdc,
                        cx + *pad,
                        cx + *w - *pad,
                        cy + line_h - ctx.ul_off,
                        *color,
                    );
                    links.push(LinkHit {
                        rect: RECT {
                            left: cx,
                            top: cy,
                            right: cx + *w,
                            bottom: cy + line_h,
                        },
                        url: url.clone(),
                    });
                }
            }
        }
    }
    SelectObject(hdc, old_font);
    (y + lines.len() as i32 * line_h, max_w)
}

/// Fill the selection background behind one laid-out line's selected words (and the spaces
/// between them), and record every word's hit rect. Runs before the line's glyphs are drawn.
pub(super) unsafe fn line_sel(
    hdc: HDC,
    toks: &[Tok],
    placed: &[(i32, usize)],
    xbase: i32,
    cy: i32,
    line_h: i32,
    sel: &mut RunSel,
) {
    let mut prev: Option<(usize, i32)> = None; // (doc end, right x) of the previous word
    for (rx, idx) in placed {
        let Tok::Word {
            w,
            pad,
            font,
            doc,
            spec,
            code,
            ..
        } = &toks[*idx]
        else {
            continue;
        };
        let Some((ds, de)) = *doc else {
            prev = None;
            continue;
        };
        let cx = xbase + rx;
        sel.hits.push(SelHit {
            rect: RECT {
                left: cx,
                top: cy,
                right: cx + *w,
                bottom: cy + line_h,
            },
            start: ds,
            end: de,
            font: *spec,
            text_x: cx + *pad,
        });
        if let Some((ss, se)) = sel.range {
            // The gap holds this line's inter-word spaces: fill it only when the selection
            // actually spans across it (so a selection ending mid-line doesn't overhang).
            if let Some((pde, prx)) = prev {
                if ss <= pde && se >= ds && prx < cx {
                    fill(hdc, prx, cy, cx, cy + line_h, sel.bg);
                }
            }
            // An inline-code span paints its own opaque panel in the selection colour (see the
            // draw loop) — filling here too would just be overpainted.
            if ss < de && se > ds && !*code {
                let (x1, x2) = if ss <= ds && se >= de {
                    (cx, cx + *w) // fully selected: the whole token box, padding included
                } else {
                    // Partly selected (a selection end lands inside this word): measure it.
                    let t = sel.doc.get(ds..de).unwrap_or("");
                    let a = ss.max(ds) - ds;
                    let b = se.min(de) - ds;
                    SelectObject(hdc, (*font).into());
                    let x = cx + *pad;
                    (
                        x + highlight::disp_extent(hdc, t, a),
                        x + highlight::disp_extent(hdc, t, b),
                    )
                };
                fill(hdc, x1, cy, x2, cy + line_h, sel.bg);
            }
        }
        prev = Some((de, cx + *w));
    }
}

/// Fill a rect with a solid colour.
pub(super) unsafe fn fill(hdc: HDC, x1: i32, y1: i32, x2: i32, y2: i32, color: u32) {
    if x2 <= x1 {
        return;
    }
    let r = RECT {
        left: x1,
        top: y1,
        right: x2,
        bottom: y2,
    };
    let b = CreateSolidBrush(COLORREF(color));
    FillRect(hdc, &r, b);
    let _ = DeleteObject(b.into());
}

/// A 1px horizontal line (strike / underline / grid) in `color`.
pub(super) unsafe fn hline(hdc: HDC, x1: i32, x2: i32, y: i32, color: u32) {
    let pen = CreatePen(PS_SOLID, 1, COLORREF(color));
    let op = SelectObject(hdc, HGDIOBJ(pen.0));
    let _ = MoveToEx(hdc, x1, y, None);
    let _ = LineTo(hdc, x2, y);
    SelectObject(hdc, op);
    let _ = DeleteObject(HGDIOBJ(pen.0));
}

/// Draw a short single-line string at `(x, y)` (list markers).
pub(super) unsafe fn draw_at(hdc: HDC, text: &str, x: i32, y: i32, font: HFONT, color: u32) {
    let old = SelectObject(hdc, font.into());
    SetTextColor(hdc, COLORREF(color));
    let mut w: Vec<u16> = text.encode_utf16().collect();
    let mut r = RECT {
        left: x,
        top: y,
        right: x + 400,
        bottom: y + 100,
    };
    DrawTextW(hdc, &mut w, &mut r, DT_LEFT | DT_TOP | DT_NOPREFIX);
    SelectObject(hdc, old);
}

/// Draw a GitHub-style task-list checkbox at `(x, y)` (its top-left), in place of a list
/// bullet. Unchecked = a rounded outline box; checked = an accent-filled box with a white
/// tick. `(x, y)` is already DPI-scaled; the box sizes itself off the body line.
pub(super) unsafe fn draw_checkbox(hwnd: HWND, hdc: HDC, x: i32, y: i32, done: bool, c: &MdColors) {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let sz = sc(14);
    let (l, t) = (x, y + sc(2)); // nudge down to sit on the 16px text line
    let (r, b) = (l + sz, t + sz);
    let rad = sc(4);
    let pen = CreatePen(
        PS_SOLID,
        sc(1).max(1),
        COLORREF(if done { c.accent } else { c.border }),
    );
    let brush = CreateSolidBrush(COLORREF(if done { c.accent } else { c.bg }));
    let op = SelectObject(hdc, HGDIOBJ(pen.0));
    let ob = SelectObject(hdc, HGDIOBJ(brush.0));
    let _ = RoundRect(hdc, l, t, r, b, rad, rad);
    SelectObject(hdc, op);
    SelectObject(hdc, ob);
    let _ = DeleteObject(HGDIOBJ(pen.0));
    let _ = DeleteObject(HGDIOBJ(brush.0));
    if done {
        // A white tick reads on the accent fill in both light and dark themes.
        let cw = CreatePen(PS_SOLID, sc(2).max(2), COLORREF(0x00FF_FFFF));
        let oc = SelectObject(hdc, HGDIOBJ(cw.0));
        let fx = |f: f32| l + (sz as f32 * f) as i32;
        let fy = |f: f32| t + (sz as f32 * f) as i32;
        let _ = MoveToEx(hdc, fx(0.24), fy(0.52), None);
        let _ = LineTo(hdc, fx(0.42), fy(0.70));
        let _ = LineTo(hdc, fx(0.76), fy(0.30));
        SelectObject(hdc, oc);
        let _ = DeleteObject(HGDIOBJ(cw.0));
    }
}

/// Re-create the font a drawn token was measured with (hit-testing; caller frees it).
pub(crate) unsafe fn font_for(hwnd: HWND, s: FontSpec) -> HFONT {
    font(hwnd, s.px, s.bold, s.italic, s.mono)
}

/// Create a font: `px` @96dpi (DPI-scaled), Segoe UI (or Consolas if `mono`), bold/italic.
pub(super) unsafe fn font(hwnd: HWND, px: i32, bold: bool, italic: bool, mono: bool) -> HFONT {
    let h = crate::win::dpi_scale(hwnd, px);
    let face = crate::win::wide(if mono { "Consolas" } else { "Segoe UI" });
    CreateFontW(
        -h,
        0,
        0,
        0,
        if bold { 700 } else { 400 },
        u32::from(italic),
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        DEFAULT_QUALITY,
        Default::default(),
        PCWSTR(face.as_ptr()),
    )
}
