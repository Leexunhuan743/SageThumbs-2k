//! Everything that turns user input into state changes: the window procedure, the
//! keyboard map, and the two commit paths (a finished drag becomes a shape, a finished
//! text buffer becomes a placed item).
//!
//! Kept apart from `paint` on purpose - this module decides WHAT is true, `paint` only
//! decides how it looks.

use super::*;

pub(super) fn pt(lparam: LPARAM) -> POINT {
    POINT {
        x: (lparam.0 & 0xffff) as u16 as i16 as i32,
        y: ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32,
    }
}

pub(super) extern "system" fn shot_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        // The Shot state is attached only after CreateWindowExW returns; any message
        // during creation has no state yet — pass it through so the deref'ing arms
        // always see a valid pointer. (WM_DESTROY guards its own null.)
        if shot_ptr(hwnd).is_null() && msg != WM_DESTROY {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        match msg {
            WM_ERASEBKGND => LRESULT(1), // the snapshot covers every pixel
            WM_LBUTTONDOWN => {
                activate_overlay(hwnd);
                let s = &mut *shot_ptr(hwnd);
                let p = pt(lparam);
                match s.sel {
                    None => {
                        if s.tool == Tool::Eyedropper {
                            // Pick a colour without dragging a region first (E + click).
                            sample_pixel(s, p);
                        } else {
                            s.sel_dragging = true;
                            s.sel_anchor = p;
                            s.cur = p;
                        }
                    }
                    Some(sel) => {
                        let dpi = shot_dpi_for_sel(s, sel);
                        let buttons = toolbar::layout(sel, s.vw, s.vh, dpi);
                        // The open colour palette intercepts clicks first.
                        if s.color_flyout {
                            if let Some((_, cbr)) =
                                buttons.iter().find(|(b, _)| *b == Button::Color)
                            {
                                let (_, sw) =
                                    toolbar::color_flyout_layout(*cbr, s.vw, s.vh, &s.customs, dpi);
                                if let Some((swatch, _)) = sw.iter().find(|(_, r)| pt_in(*r, p)) {
                                    match *swatch {
                                        Swatch::Color(c) | Swatch::Custom(Some(c)) => {
                                            s.cur_color = c
                                        }
                                        Swatch::Custom(None) | Swatch::Picker => {
                                            pick_custom_color(hwnd, s)
                                        }
                                    }
                                    s.color_flyout = false;
                                    let _ = InvalidateRect(Some(hwnd), None, false);
                                    return LRESULT(0);
                                }
                            }
                            // Clicked off the palette → close it; consume if that click
                            // was the Colour button itself (else fall through).
                            s.color_flyout = false;
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            if toolbar::hit(&buttons, p.x, p.y) == Some(Button::Color) {
                                return LRESULT(0);
                            }
                        }
                        // The open text settings flyout intercepts clicks too.
                        if s.text_flyout {
                            if let Some((_, tbr)) =
                                buttons.iter().find(|(b, _)| *b == Button::Tool(Tool::Text))
                            {
                                let (_, its) = toolbar::text_flyout_layout(
                                    *tbr,
                                    s.vw,
                                    s.vh,
                                    s.font_dropdown,
                                    dpi,
                                );
                                if let Some((item, _)) = its.iter().find(|(_, r)| pt_in(*r, p)) {
                                    match *item {
                                        TextItem::FontField => s.font_dropdown = !s.font_dropdown,
                                        TextItem::FontOption(i) => {
                                            tools::set_face(
                                                &mut s.text_font,
                                                toolbar::PRESET_FONTS[i],
                                            );
                                            s.font_dropdown = false;
                                        }
                                        TextItem::SizeDown => {
                                            let sz = (-s.text_font.lfHeight - 2).max(8);
                                            s.text_font.lfHeight = -sz;
                                        }
                                        TextItem::SizeUp => {
                                            let sz = (-s.text_font.lfHeight + 2).min(120);
                                            s.text_font.lfHeight = -sz;
                                        }
                                        TextItem::Bold => {
                                            s.text_font.lfWeight = if s.text_font.lfWeight >= 700 {
                                                400
                                            } else {
                                                700
                                            };
                                        }
                                        TextItem::Underline => {
                                            s.text_font.lfUnderline =
                                                u8::from(s.text_font.lfUnderline == 0);
                                        }
                                        TextItem::More => {
                                            pick_text_font(hwnd, s);
                                            s.text_flyout = false;
                                            s.font_dropdown = false;
                                        }
                                    }
                                    let _ = InvalidateRect(Some(hwnd), None, false);
                                    return LRESULT(0);
                                }
                            }
                            // Clicked off the flyout → close it. Consume if it was the
                            // Text button itself; else fall through (a canvas click
                            // then drops the text caret and starts typing).
                            s.text_flyout = false;
                            s.font_dropdown = false;
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            if toolbar::hit(&buttons, p.x, p.y) == Some(Button::Tool(Tool::Text)) {
                                return LRESULT(0);
                            }
                        }
                        // A click on a toolbar button takes priority over drawing.
                        if let Some(btn) = toolbar::hit(&buttons, p.x, p.y) {
                            if handle_button(hwnd, s, btn) {
                                return LRESULT(0); // window destroyed — stop touching it
                            }
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                        let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
                        if ctrl && s.typing.is_some() && s.tool == Tool::Text {
                            // Ctrl-drag while typing repositions the *active* text box
                            // (you stay in edit mode) — place the caption as you write it.
                            s.typing_drag = true;
                            s.move_from = Some(p);
                        } else if ctrl || s.tool == Tool::Move {
                            // Move tool — or Ctrl-drag with any tool — grabs the
                            // topmost shape under the cursor (if any).
                            s.selected = tools::hit_shape(&s.shapes, p.x, p.y);
                            s.move_from = s.selected.map(|_| p);
                        } else if s.tool == Tool::Eyedropper {
                            sample_pixel(s, p); // grab the pixel's colour; never draws
                        } else if s.tool == Tool::Text {
                            // Click while typing = finish & deselect (no new box on this
                            // click); a click when idle starts a fresh box. Predictable
                            // "click away to commit" instead of spawning an empty box you
                            // then have to Esc out of.
                            if s.typing.is_some() {
                                commit_text(s);
                            } else {
                                s.typing = Some((p, String::new()));
                                s.pending_hi = None; // fresh buffer, no half-typed surrogate
                            }
                        } else if s.tool == Tool::Number {
                            let n = s.number_next;
                            s.number_next += 1;
                            let color = s.color();
                            s.shapes.push(Shape::Number { at: p, n, color });
                            s.redo.clear();
                        } else {
                            s.draw_from = Some(p);
                            s.pen_pts.clear();
                            s.pen_pts.push(p);
                            s.cur = p;
                        }
                    }
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let s = &mut *shot_ptr(hwnd);
                let p = pt(lparam);
                // The Eyedropper loupe tracks the cursor: clear the "copied" flash and
                // repaint just the old + new loupe areas (not the whole virtual screen,
                // which would be a heavy blit per tick on a multi-monitor desktop).
                if s.tool == Tool::Eyedropper {
                    let old = loupe_rect(s, s.cur.x, s.cur.y);
                    let new = loupe_rect(s, p.x, p.y);
                    s.eye_copied = false;
                    let _ = InvalidateRect(Some(hwnd), Some(&old), false);
                    let _ = InvalidateRect(Some(hwnd), Some(&new), false);
                }
                s.cur = p;
                if s.sel_dragging {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else if s.typing_drag {
                    // Reposition the active text box by the cursor delta (still editing).
                    if let Some(from) = s.move_from {
                        if let Some((at, _)) = s.typing.as_mut() {
                            at.x += p.x - from.x;
                            at.y += p.y - from.y;
                        }
                        s.move_from = Some(p);
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else if let (Some(from), Some(idx)) = (s.move_from, s.selected) {
                    // Drag the grabbed shape by the cursor delta.
                    let (dx, dy) = (p.x - from.x, p.y - from.y);
                    if idx < s.shapes.len() {
                        tools::translate_shape(&mut s.shapes[idx], dx, dy);
                    }
                    s.move_from = Some(p);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else if s.draw_from.is_some() {
                    if s.tool == Tool::Pen {
                        s.pen_pts.push(p);
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                // Track which toolbar button we're hovering (only when idle), and
                // (re)arm the hover-delay timer so the tooltip pops after a beat.
                let idle = !s.sel_dragging && s.draw_from.is_none() && s.move_from.is_none();
                let hovered = match (idle, s.sel) {
                    (true, Some(sel)) => toolbar::hit(
                        &toolbar::layout(sel, s.vw, s.vh, shot_dpi_for_sel(s, sel)),
                        p.x,
                        p.y,
                    ),
                    _ => None,
                };
                if hovered != s.hover_btn {
                    s.hover_btn = hovered;
                    s.tip_show = false;
                    let _ = KillTimer(Some(hwnd), HOVER_TIMER);
                    if hovered.is_some() {
                        let _ = SetTimer(Some(hwnd), HOVER_TIMER, 450, None);
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let s = &mut *shot_ptr(hwnd);
                let p = pt(lparam);
                if s.sel_dragging {
                    s.sel_dragging = false;
                    let r = tools::norm(s.sel_anchor, p);
                    if (r.right - r.left) > 4 && (r.bottom - r.top) > 4 {
                        s.sel = Some(r);
                        // OCR launch mode: the region IS the whole gesture. Recognize and
                        // close instead of raising the annotation toolbar. A too-small drag
                        // falls through with no selection, so the overlay stays up for a
                        // second try rather than closing on a mis-click.
                        if s.ocr_mode {
                            finish_ocr(s);
                            let _ = DestroyWindow(hwnd);
                            return LRESULT(0);
                        }
                    }
                } else if s.typing_drag {
                    s.typing_drag = false;
                    s.move_from = None; // done repositioning the active text box
                } else if s.move_from.is_some() {
                    s.move_from = None; // finished dragging the selected shape
                } else if let Some(a) = s.draw_from.take() {
                    let shift = shift_active(s);
                    let tool = s.tool;
                    let final_point = tools::drag_endpoint(tool, a, p, shift);
                    if finish_shape(s, a, final_point) {
                        if let Some(state) = s.automation.as_mut() {
                            state.commit_gen += 1;
                            state.last_drag = Some(AutomationDrag {
                                tool,
                                anchor: a,
                                raw: p,
                                final_point,
                                snapped: shift,
                            });
                            state.status = "ready";
                        }
                    }
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_CHAR => {
                let s = &mut *shot_ptr(hwnd);
                // WM_CHAR carries one UTF-16 code unit — decode it (not a single ASCII
                // byte) so accented and other Unicode characters type correctly. A
                // non-BMP character arrives as a high+low surrogate pair across two
                // messages; buffer the high half until its low half lands.
                if s.typing.is_some() {
                    let u = (wparam.0 & 0xFFFF) as u16;
                    if let Some(hi) = s.pending_hi.take() {
                        // Expecting the low half of a surrogate pair.
                        if (0xDC00..=0xDFFF).contains(&u) {
                            if let Some(ch) =
                                char::decode_utf16([hi, u]).next().and_then(|r| r.ok())
                            {
                                if let Some((_, buf)) = s.typing.as_mut() {
                                    buf.push(ch);
                                }
                            }
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                        // Stray high surrogate without a matching low half — drop it and
                        // fall through to process `u` on its own.
                    }
                    if (0xD800..=0xDBFF).contains(&u) {
                        s.pending_hi = Some(u); // high surrogate — wait for its low half
                    } else if u == 0x08 {
                        if let Some((_, buf)) = s.typing.as_mut() {
                            buf.pop();
                        }
                    } else if u >= 0x20 {
                        // A BMP character (lone surrogates were handled above). Use the
                        // lossy path so an unexpected unpaired surrogate can't panic.
                        if let Some((_, buf)) = s.typing.as_mut() {
                            buf.push_str(&String::from_utf16_lossy(&[u]));
                        }
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    return LRESULT(0);
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let vk = wparam.0 as u16;
                // Bit 30 means the key was already down. A held F8 must toggle the
                // automation latch once, not on every auto-repeat WM_KEYDOWN.
                let repeated_f8 = vk == VK_F8.0 && (lparam.0 & (1isize << 30)) != 0;
                let shift_preview = if vk == VK_SHIFT.0 {
                    let s = &*shot_ptr(hwnd);
                    s.draw_from.is_some() && matches!(s.tool, Tool::Line | Tool::Arrow)
                } else {
                    false
                };
                if (!repeated_f8 && handle_key(hwnd, vk)) || shift_preview {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_KEYUP => {
                // Shift can be pressed or released without moving the mouse. Repaint an
                // active line/arrow so its preview toggles immediately in either direction.
                if wparam.0 as u16 == VK_SHIFT.0 {
                    let s = &*shot_ptr(hwnd);
                    if s.draw_from.is_some() && matches!(s.tool, Tool::Line | Tool::Arrow) {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                LRESULT(0)
            }
            WM_TIMER => {
                let s = &mut *shot_ptr(hwnd);
                if wparam.0 == HOVER_TIMER {
                    let _ = KillTimer(Some(hwnd), HOVER_TIMER);
                    if s.hover_btn.is_some() && !s.tip_show {
                        s.tip_show = true;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                // Only override the client area; let the default handle the rest.
                if (lparam.0 & 0xffff) as u32 != HTCLIENT {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let s = &*shot_ptr(hwnd);
                let p = s.cur; // last client-space mouse pos (WM_SETCURSOR precedes the move)
                let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
                // Over the toolbar, or over an open flyout panel?
                let over_ui = s.sel.is_some_and(|sel| {
                    let dpi = shot_dpi_for_sel(s, sel);
                    let buttons = toolbar::layout(sel, s.vw, s.vh, dpi);
                    if toolbar::hit(&buttons, p.x, p.y).is_some() {
                        return true;
                    }
                    if s.color_flyout {
                        if let Some((_, cbr)) = buttons.iter().find(|(b, _)| *b == Button::Color) {
                            let (panel, _) =
                                toolbar::color_flyout_layout(*cbr, s.vw, s.vh, &s.customs, dpi);
                            return pt_in(panel, p);
                        }
                    }
                    if s.text_flyout {
                        if let Some((_, tbr)) =
                            buttons.iter().find(|(b, _)| *b == Button::Tool(Tool::Text))
                        {
                            let (panel, _) =
                                toolbar::text_flyout_layout(*tbr, s.vw, s.vh, s.font_dropdown, dpi);
                            return pt_in(panel, p);
                        }
                    }
                    false
                });
                let moving = ctrl || s.tool == Tool::Move;
                let over_shape = moving && tools::hit_shape(&s.shapes, p.x, p.y).is_some();
                let active_typing_move = ctrl && s.tool == Tool::Text && s.typing.is_some();
                // An already-active gesture wins over modifier changes and toolbar
                // hover. When idle, match the pointer to what the next click would do.
                let id = if s.typing_drag || s.move_from.is_some() {
                    IDC_SIZEALL
                } else if s.sel_dragging || s.draw_from.is_some() {
                    IDC_CROSS
                } else if over_ui {
                    IDC_ARROW
                } else if active_typing_move || over_shape {
                    IDC_SIZEALL
                } else if moving {
                    IDC_ARROW
                } else if s.tool == Tool::Text {
                    IDC_IBEAM
                } else {
                    IDC_CROSS
                };
                if let Ok(cur) = LoadCursorW(None, id) {
                    SetCursor(Some(cur));
                }
                LRESULT(1)
            }
            WM_PAINT => {
                shot_paint(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let ptr = shot_ptr(hwnd);
                if !ptr.is_null() {
                    let s = Box::from_raw(ptr);
                    let _ = DeleteDC(s.shot);
                    let _ = DeleteObject(HGDIOBJ(s.shot_bmp.0));
                    let _ = DeleteDC(s.dimmed);
                    let _ = DeleteObject(HGDIOBJ(s.dimmed_bmp.0));
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Keyboard: tool shortcuts, colour/thickness, undo/redo, accept (Enter → copy),
/// save (Ctrl+S), cancel/close (Esc). Returns true if a repaint is needed.
pub(super) unsafe fn handle_key(hwnd: HWND, vk: u16) -> bool {
    let s = &mut *shot_ptr(hwnd);
    let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
    let shift = (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;

    // Windows automation can send a drag or a key chord, but cannot hold a
    // modifier across a drag. This automation-only latch lets a real mouse-message
    // drag exercise the exact same Shift-snap preview/commit path.
    if vk == VK_F8.0 {
        if let Some(state) = s.automation.as_mut() {
            state.forced_shift = !state.forced_shift;
            state.status = "ready";
            return true;
        }
    }

    // Ignore the close keys for a moment after the overlay opens, so the keystroke
    // that fired the launching hotkey can't instantly cancel/accept the capture.
    if (vk == VK_ESCAPE.0 || vk == VK_RETURN.0)
        && GetTickCount64().saturating_sub(s.born) < SETTLE_CLOSE_MS
    {
        return false;
    }

    if vk == VK_ESCAPE.0 {
        // Peel back transient editor state before closing the whole capture. This
        // makes Esc useful for correcting a mis-click instead of immediately losing
        // the screenshot underneath it.
        if s.sel_dragging {
            s.sel_dragging = false; // cancel the initial region drag, keep the overlay
            return true;
        }
        if s.color_flyout || s.text_flyout || s.font_dropdown {
            s.color_flyout = false;
            s.text_flyout = false;
            s.font_dropdown = false;
            return true;
        }
        if s.typing.is_some() {
            s.typing = None; // cancel the in-progress text only
            s.typing_drag = false; // and any active reposition drag
            s.move_from = None;
            s.pending_hi = None; // drop any half-typed surrogate
            return true;
        }
        if s.draw_from.take().is_some() {
            s.pen_pts.clear(); // cancel the active annotation, keep the capture
            return true;
        }
        if s.selected.take().is_some() {
            s.move_from = None; // deselect first; a second Esc closes
            return true;
        }
        let _ = DestroyWindow(hwnd);
        return false;
    }
    if vk == VK_RETURN.0 {
        if block_automation_output(s, "blocked-copy") {
            return true;
        }
        // Enter = accept to the clipboard (the quick "I'm done, it's copied" gesture).
        // Saving to a file is the explicit Ctrl+S / Save-button action.
        commit_text(s);
        if s.sel.is_some() {
            finish_copy(s);
        }
        let _ = DestroyWindow(hwnd);
        return false;
    }
    // While typing text, swallow every other key here (the characters are inserted
    // by WM_CHAR) so letters go into the text instead of triggering tool shortcuts.
    if s.typing.is_some() {
        return false;
    }

    // Move tool: Delete removes the grabbed shape.
    if vk == VK_DELETE.0 {
        if let Some(idx) = s.selected.take() {
            if idx < s.shapes.len() {
                s.shapes.remove(idx);
                s.redo.clear();
            }
        }
        s.move_from = None;
        return true;
    }

    if ctrl && !shift && vk == b'Z' as u16 {
        if let Some(sh) = s.shapes.pop() {
            s.redo.push(sh);
        }
        s.selected = None; // indices may have shifted
        s.move_from = None;
        return true;
    }
    if ctrl && (vk == b'Y' as u16 || (shift && vk == b'Z' as u16)) {
        if let Some(sh) = s.redo.pop() {
            s.shapes.push(sh);
        }
        s.selected = None;
        s.move_from = None;
        return true;
    }
    // Ctrl+C = copy to clipboard; Ctrl+S = save. Both accept + close (only once a
    // region exists). Ctrl+S keeps the overlay open if the Save-As prompt is cancelled.
    // Checked before the plain-letter tool shortcuts below, so 'C' alone is still Ellipse.
    if ctrl && vk == b'C' as u16 {
        if block_automation_output(s, "blocked-copy") {
            return true;
        }
        if s.sel.is_some() {
            commit_text(s);
            finish_copy(s);
            let _ = DestroyWindow(hwnd);
        }
        return false;
    }
    // Ctrl+T = read the region's text (OCR) and close. Also checked ahead of the
    // plain-letter shortcuts, so 'T' alone stays the Text tool.
    if ctrl && vk == b'T' as u16 {
        if block_automation_output(s, "blocked-ocr") {
            return true;
        }
        if s.sel.is_some() {
            commit_text(s);
            finish_ocr(s);
            let _ = DestroyWindow(hwnd);
        }
        return false;
    }
    if ctrl && vk == b'S' as u16 {
        if block_automation_output(s, "blocked-save") {
            return true;
        }
        if s.sel.is_some() {
            commit_text(s);
            if finish_save(hwnd, s) {
                let _ = DestroyWindow(hwnd);
            }
        }
        return false;
    }

    // OCR launch mode never reaches the annotation pass, so the tool / colour / thickness
    // shortcuts below have nothing to act on — and the Eyedropper's pick-without-a-region
    // click would hijack the one drag the mode exists for. Swallow them.
    if s.ocr_mode {
        return false;
    }

    let new_tool = match vk {
        x if x == b'R' as u16 => Some(Tool::Rect),
        x if x == b'O' as u16 || x == b'C' as u16 => Some(Tool::Ellipse),
        x if x == b'A' as u16 => Some(Tool::Arrow),
        x if x == b'L' as u16 => Some(Tool::Line),
        x if x == b'P' as u16 => Some(Tool::Pen),
        x if x == b'T' as u16 => Some(Tool::Text),
        x if x == b'N' as u16 => Some(Tool::Number),
        x if x == b'H' as u16 => Some(Tool::Highlight),
        x if x == b'B' as u16 => Some(Tool::Pixelate), // B = blur/blockify
        x if x == b'I' as u16 => Some(Tool::Invert),
        x if x == b'E' as u16 => Some(Tool::Eyedropper),
        x if x == b'M' as u16 => Some(Tool::Move),
        _ => None,
    };
    if let Some(t) = new_tool {
        commit_text(s);
        s.tool = t;
        s.selected = None; // dropping the move selection when switching tools
        s.move_from = None;
        s.typing_drag = false;
        return true;
    }
    if vk == b'K' as u16 {
        s.cycle_color();
        return true;
    }
    if vk == 0xDB {
        // VK_OEM_4 '[' — text size while the Text tool is active, else line thickness.
        if s.tool == Tool::Text {
            let sz = (-s.text_font.lfHeight - 2).max(10);
            s.text_font.lfHeight = -sz;
        } else {
            s.thickness = (s.thickness - 1).max(1);
        }
        return true;
    }
    if vk == 0xDD {
        // VK_OEM_6 ']'
        if s.tool == Tool::Text {
            let sz = (-s.text_font.lfHeight + 2).min(96);
            s.text_font.lfHeight = -sz;
        } else {
            s.thickness = (s.thickness + 1).min(40);
        }
        return true;
    }
    false
}

/// Turn the finished drag (anchor `a` → release `b`) into a [`Shape`]. Returns
/// whether a shape was actually committed (tiny/unsupported gestures return false).
pub(super) fn finish_shape(s: &mut Shot, a: POINT, b: POINT) -> bool {
    let color = s.color();
    let w = s.thickness;
    let shape = match s.tool {
        Tool::Rect => Shape::Rect {
            r: tools::norm(a, b),
            color,
            w,
        },
        Tool::Ellipse => Shape::Ellipse {
            r: tools::norm(a, b),
            color,
            w,
        },
        Tool::Arrow => Shape::Arrow { a, b, color, w },
        Tool::Line => Shape::Line { a, b, color, w },
        Tool::Pen => Shape::Pen {
            pts: std::mem::take(&mut s.pen_pts),
            color,
            w,
        },
        Tool::Highlight => Shape::Highlight {
            r: tools::norm(a, b),
            color,
        },
        Tool::Pixelate => Shape::Pixelate {
            r: tools::norm(a, b),
        },
        Tool::Invert => Shape::Invert {
            r: tools::norm(a, b),
        },
        Tool::Text | Tool::Number | Tool::Eyedropper | Tool::Move => return false,
    };
    // Skip a tiny accidental drag for any rect-based shape.
    if matches!(&shape,
        Shape::Rect { r, .. } | Shape::Ellipse { r, .. } | Shape::Highlight { r, .. }
            | Shape::Pixelate { r } | Shape::Invert { r }
        if (r.right - r.left).abs() < 3 && (r.bottom - r.top).abs() < 3)
    {
        return false;
    }
    s.shapes.push(shape);
    s.redo.clear();
    true
}

/// Commit a non-empty active text buffer into a placed Text shape.
pub(super) fn commit_text(s: &mut Shot) {
    s.pending_hi = None; // any half-typed surrogate is abandoned when the buffer closes
    if let Some((at, buf)) = s.typing.take() {
        if !buf.is_empty() {
            let color = s.color();
            let font = s.text_font;
            s.shapes.push(Shape::Text {
                at,
                s: buf,
                color,
                font,
            });
            s.redo.clear();
        }
    }
}
