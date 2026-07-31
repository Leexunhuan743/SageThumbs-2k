//! GFM table layout and drawing.
//!
//! Column widths come from a two-pass measure: pass one takes each column's natural
//! (unwrapped) width AND its minimum (the widest single word, found by a width=1 dry
//! run), pass two wraps every cell at the width finally chosen. That is what lets a wide
//! table shrink gracefully instead of either overflowing or breaking mid-word.

use super::*;

/// Draw one GFM/HTML table GitHub-style: full 1px grid, bold header, zebra body rows,
/// per-column alignment, auto column widths (natural, proportionally shrunk to fit).
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
pub(super) unsafe fn draw_table(
    hwnd: HWND,
    hdc: HDC,
    header: &[Vec<Run>],
    rows: &[Vec<Vec<Run>>],
    aligns: &[u8],
    x0: i32,
    y0: i32,
    avail: i32,
    c: &MdColors,
    links: &mut Vec<LinkHit>,
    sel: &mut TblSel,
) -> i32 {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let ncols = header
        .len()
        .max(rows.iter().map(|r| r.len()).max().unwrap_or(0))
        .max(1);
    let hpad = sc(13);
    let vpad = sc(6);
    let fonts = Fonts::new(hwnd, BODY_PX, false, false);
    let hfonts = Fonts::new(hwnd, BODY_PX, true, false);
    let ctx = ctx_for(hwnd, c, c.fg);
    let col_align = |ci: usize| aligns.get(ci).copied().unwrap_or(0);

    // Every row to draw, in order, with its "is the header row" flag (HTML tables may have none).
    let all: Vec<(&[Vec<Run>], bool)> = {
        let mut v: Vec<(&[Vec<Run>], bool)> = Vec::with_capacity(rows.len() + 1);
        if !header.is_empty() {
            v.push((header, true));
        }
        v.extend(rows.iter().map(|r| (r.as_slice(), false)));
        v
    };

    // Pass 1: per-column natural (unwrapped) and minimum (widest single word) widths — the
    // width=1 dry pass forces a wrap at every word, so its widest line IS the widest word.
    let mut nat = vec![sc(24); ncols];
    let mut minw = vec![sc(24); ncols];
    let mut scratch: Vec<LinkHit> = Vec::new();
    for (row, is_hdr) in &all {
        let f = if *is_hdr { &hfonts } else { &fonts };
        for (ci, cell) in row.iter().enumerate().take(ncols) {
            let (_, w) = run_block(
                hdc,
                cell,
                f,
                0,
                0,
                i32::MAX / 4,
                0,
                true,
                &ctx,
                &mut scratch,
                None,
            );
            nat[ci] = nat[ci].max(w + 2 * hpad);
            let (_, mw) = run_block(hdc, cell, f, 0, 0, 1, 0, true, &ctx, &mut scratch, None);
            minw[ci] = minw[ci].max(mw + 2 * hpad);
        }
    }
    // Browser-style auto layout: everything at natural width if it fits; otherwise every column
    // keeps at least its min-content width and the slack is distributed in proportion to how
    // much each column WANTS to grow (nat - min). Only when even the minimums overflow do we
    // shrink below min (proportionally, clipped at the pane edge).
    let sum_nat: i64 = nat.iter().map(|w| *w as i64).sum();
    let sum_min: i64 = minw.iter().map(|w| *w as i64).sum();
    let colw: Vec<i32> = if sum_nat <= avail as i64 {
        nat
    } else if sum_min >= avail as i64 {
        minw.iter()
            .map(|w| ((*w as i64 * avail as i64 / sum_min.max(1)) as i32).max(sc(40)))
            .collect()
    } else {
        let slack = avail as i64 - sum_min;
        let want: i64 = sum_nat - sum_min;
        nat.iter()
            .zip(&minw)
            .map(|(n, m)| (*m as i64 + (*n - *m) as i64 * slack / want.max(1)) as i32)
            .collect()
    };
    let table_w: i32 = colw.iter().sum::<i32>().min(avail);
    let cell_x = |ci: usize| x0 + colw[..ci].iter().sum::<i32>();

    // Pass 2: row heights (wrap each cell at its column width).
    let line_h_probe = {
        let old = SelectObject(hdc, fonts.reg.into());
        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(hdc, &mut tm);
        SelectObject(hdc, old);
        (tm.tmHeight + tm.tmExternalLeading + sc(3)).max(1)
    };
    let mut row_h: Vec<i32> = Vec::new();
    for (row, is_hdr) in &all {
        let f = if *is_hdr { &hfonts } else { &fonts };
        let mut h = line_h_probe;
        for (ci, cell) in row.iter().enumerate().take(ncols) {
            let w = (colw[ci] - 2 * hpad).max(sc(24));
            let (ny, _) = run_block(hdc, cell, f, 0, 0, w, 0, true, &ctx, &mut scratch, None);
            h = h.max(ny);
        }
        row_h.push(h + 2 * vpad);
    }

    // Pass 3: draw. Zebra fill first, then text, then the grid on top.
    let mut y = y0;
    let mut body_i = 0usize;
    for (ri, (row, is_hdr)) in all.iter().enumerate() {
        let f = if *is_hdr { &hfonts } else { &fonts };
        let h = row_h[ri];
        if !is_hdr {
            // GitHub zebra: every 2nd body row gets the subtle fill.
            if body_i % 2 == 1 {
                let zr = RECT {
                    left: x0,
                    top: y,
                    right: x0 + table_w,
                    bottom: y + h,
                };
                let zb = CreateSolidBrush(COLORREF(c.code_bg));
                FillRect(hdc, &zr, zb);
                let _ = DeleteObject(zb.into());
            }
            body_i += 1;
        }
        for (ci, cell) in row.iter().enumerate().take(ncols) {
            let w = (colw[ci] - 2 * hpad).max(sc(24));
            let mut rsel = RunSel {
                range: sel.range,
                doc: sel.doc,
                bases: sel
                    .bases
                    .get(ri)
                    .and_then(|r| r.get(ci))
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]),
                hits: &mut *sel.hits,
                bg: sel.bg,
            };
            let _ = run_block(
                hdc,
                cell,
                f,
                cell_x(ci) + hpad,
                y + vpad,
                w,
                col_align(ci),
                false,
                &ctx,
                links,
                Some(&mut rsel),
            );
        }
        y += h;
        hline(hdc, x0, x0 + table_w, y, c.border); // row separator
    }
    // top edge + verticals
    hline(hdc, x0, x0 + table_w, y0, c.border);
    for ci in 0..=ncols {
        let x = if ci == ncols {
            x0 + table_w
        } else {
            cell_x(ci)
        };
        let pen = CreatePen(PS_SOLID, 1, COLORREF(c.border));
        let op = SelectObject(hdc, HGDIOBJ(pen.0));
        let _ = MoveToEx(hdc, x, y0, None);
        let _ = LineTo(hdc, x, y);
        SelectObject(hdc, op);
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
    fonts.free();
    hfonts.free();
    y
}

/// Selection wiring for one [`draw_table`] call (per-cell [`RunSel`]s are built from it).
pub(super) struct TblSel<'a> {
    pub(super) range: Option<(usize, usize)>,
    pub(super) doc: &'a str,
    pub(super) bases: &'a [Vec<Vec<usize>>],
    pub(super) hits: &'a mut Vec<SelHit>,
    pub(super) bg: u32,
}
