#![windows_subsystem = "windows"]
//! DESKTOUCH : un petit carré discret sur le bureau qui s'ouvre en panneau de raccourcis au survol.
//! Zéro CPU au repos : le minuteur d'animation ne tourne que pendant une transition.

use std::{cell::RefCell, mem::zeroed, ptr::null};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    System::{LibraryLoader::GetModuleHandleW, SystemInformation::GetTickCount64},
    UI::{Controls::MARGINS, Input::KeyboardAndMouse::*, Shell::ShellExecuteW, WindowsAndMessaging::*},
};

const WM_MOUSELEAVE: u32 = 0x02A3; // absent de windows-sys
const X: i32 = 60;
const Y: i32 = 120;
const SMALL: f32 = 44.0;
const TILE: i32 = 92;
const GAP: i32 = 10;
const PAD: i32 = 14;
const COLS: i32 = 3;
const BIG_W: f32 = (PAD * 2 + TILE * COLS + GAP * (COLS - 1)) as f32;
const BIG_H: f32 = (PAD * 2 + TILE * 2 + GAP) as f32;
const IDLE_ALPHA: f32 = 80.0;

// (glyphe Segoe Fluent Icons, libellé, cible ShellExecute)
const ITEMS: [(u16, &str, &str); 6] = [
    (0xE8B7, "Fichiers", "explorer.exe"),
    (0xE774, "Web", "https://www.google.com"),
    (0xE7FC, "Jeux", "steam://open/main"),
    (0xE756, "Terminal", "wt.exe"),
    (0xE8EF, "Calculatrice", "calc.exe"),
    (0xE713, "Réglages", "ms-settings:"),
];

struct State {
    t: f32,
    target: f32,
    last: u64,
    hover: i32,
    tracking: bool,
    icon_font: HFONT,
    text_font: HFONT,
}

thread_local! {
    static ST: RefCell<State> = const { RefCell::new(State {
        t: 0.0, target: 0.0, last: 0, hover: -1, tracking: false,
        icon_font: null_font(), text_font: null_font(),
    }) };
}

const fn null_font() -> HFONT {
    0 as HFONT
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn rgb(v: f32) -> u32 {
    let c = v.clamp(0.0, 255.0) as u32;
    c | c << 8 | c << 16
}

/// Index de la tuile sous (x, y), ou -1.
fn hit(x: i32, y: i32) -> i32 {
    let (cx, cy) = (x - PAD, y - PAD);
    if cx < 0 || cy < 0 || cx % (TILE + GAP) >= TILE || cy % (TILE + GAP) >= TILE {
        return -1;
    }
    let (col, row) = (cx / (TILE + GAP), cy / (TILE + GAP));
    let i = row * COLS + col;
    if col < COLS && (i as usize) < ITEMS.len() { i } else { -1 }
}

fn tile_rect(i: i32) -> RECT {
    let (x, y) = (PAD + (i % COLS) * (TILE + GAP), PAD + (i / COLS) * (TILE + GAP));
    RECT { left: x, top: y, right: x + TILE, bottom: y + TILE }
}

unsafe fn animate(h: HWND, target: f32) {
    ST.with_borrow_mut(|s| {
        s.target = target;
        s.last = GetTickCount64();
    });
    SetTimer(h, 1, 8, None);
}

unsafe fn step(h: HWND) {
    let t = ST.with_borrow_mut(|s| {
        let now = GetTickCount64();
        let dt = (now - s.last) as f32;
        s.last = now;
        // approche exponentielle : indépendante du nombre d'images, ralentit en douceur
        s.t += (s.target - s.t) * (1.0 - (-dt / 70.0).exp());
        if (s.target - s.t).abs() < 0.002 {
            s.t = s.target;
        }
        s.t
    });
    if ST.with_borrow(|s| s.t == s.target) {
        KillTimer(h, 1);
    }
    let size = |a: f32, b: f32| (a + (b - a) * t).round() as i32;
    SetWindowPos(h, null_mut(), 0, 0, size(SMALL, BIG_W), size(SMALL, BIG_H),
        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
    SetLayeredWindowAttributes(h, 0, (IDLE_ALPHA + (255.0 - IDLE_ALPHA) * t.min(1.0) * 2.0).min(255.0) as u8, LWA_ALPHA);
    InvalidateRect(h, null(), 0);
}

fn null_mut() -> HWND {
    std::ptr::null_mut()
}

unsafe fn paint(h: HWND) {
    let mut ps: PAINTSTRUCT = zeroed();
    let dc = BeginPaint(h, &mut ps);
    let mut rc: RECT = zeroed();
    GetClientRect(h, &mut rc);
    let mem = CreateCompatibleDC(dc);
    let bmp = CreateCompatibleBitmap(dc, rc.right, rc.bottom);
    let old_bmp = SelectObject(mem, bmp);
    // noir = transparent : DWM y laisse voir le flou acrylique
    PatBlt(mem, 0, 0, rc.right, rc.bottom, BLACKNESS);
    SetBkMode(mem, TRANSPARENT as i32);
    let old_pen = SelectObject(mem, GetStockObject(NULL_PEN));

    let (t, hover, icon_font, text_font) = ST.with_borrow(|s| (s.t, s.hover, s.icon_font, s.text_font));
    // les couleurs s'additionnent au verre : baisser l'intensité revient à faire un fondu
    let open = ((t - 0.45) / 0.55).clamp(0.0, 1.0);
    let closed = (1.0 - t * 2.5).clamp(0.0, 1.0);

    if closed > 0.0 {
        let b = CreateSolidBrush(rgb(210.0 * closed));
        let old_b = SelectObject(mem, b);
        for (dx, dy) in [(13, 13), (23, 13), (13, 23), (23, 23)] {
            RoundRect(mem, dx, dy, dx + 9, dy + 9, 4, 4);
        }
        SelectObject(mem, old_b);
        DeleteObject(b);
    }

    if open > 0.0 {
        for (i, (glyph, label, _)) in ITEMS.iter().enumerate() {
            let r = tile_rect(i as i32);
            let b = CreateSolidBrush(rgb(if hover == i as i32 { 62.0 } else { 26.0 } * open));
            let old_b = SelectObject(mem, b);
            RoundRect(mem, r.left, r.top, r.right + 1, r.bottom + 1, 22, 22);
            SelectObject(mem, old_b);
            DeleteObject(b);

            SelectObject(mem, icon_font);
            SetTextColor(mem, rgb(240.0 * open));
            let mut ri = RECT { bottom: r.top + 58, top: r.top + 12, ..r };
            DrawTextW(mem, glyph, 1, &mut ri, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

            SelectObject(mem, text_font);
            SetTextColor(mem, rgb(200.0 * open));
            let txt: Vec<u16> = label.encode_utf16().collect();
            let mut rt = RECT { top: r.top + 60, bottom: r.bottom - 8, ..r };
            DrawTextW(mem, txt.as_ptr(), txt.len() as i32, &mut rt,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        }
    }

    BitBlt(dc, 0, 0, rc.right, rc.bottom, mem, 0, 0, SRCCOPY);
    SelectObject(mem, old_pen);
    SelectObject(mem, old_bmp);
    DeleteObject(bmp);
    DeleteDC(mem);
    EndPaint(h, &ps);
}

unsafe fn font(size: i32, weight: i32, name: &str) -> HFONT {
    CreateFontW(-size, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET as u32, 0, 0,
        ANTIALIASED_QUALITY as u32, 0, w(name).as_ptr())
}

unsafe fn set_glass(h: HWND) {
    let attr = |a: DWMWINDOWATTRIBUTE, v: i32| {
        DwmSetWindowAttribute(h, a as u32, &v as *const i32 as _, 4);
    };
    attr(DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
    attr(DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND);
    attr(DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_TRANSIENTWINDOW); // acrylique
    let m = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
    DwmExtendFrameIntoClientArea(h, &m);
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            set_glass(h);
            ST.with_borrow_mut(|s| {
                s.icon_font = font(30, 400, "Segoe Fluent Icons");
                s.text_font = font(13, 500, "Segoe UI Variable Text");
            });
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            paint(h);
            0
        }
        // ne jamais voler le focus ni passer devant les autres fenêtres
        WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
        WM_MOUSEMOVE => {
            let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
            let start = ST.with_borrow_mut(|s| !std::mem::replace(&mut s.tracking, true));
            if start {
                let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                TrackMouseEvent(&mut tme);
                KillTimer(h, 2);
                animate(h, 1.0);
            }
            let changed = ST.with_borrow_mut(|s| {
                let hv = if s.t > 0.9 { hit(x, y) } else { -1 };
                std::mem::replace(&mut s.hover, hv) != hv
            });
            if changed {
                InvalidateRect(h, null(), 0);
            }
            0
        }
        WM_MOUSELEAVE => {
            ST.with_borrow_mut(|s| {
                s.tracking = false;
                s.hover = -1;
            });
            SetTimer(h, 2, 350, None); // petit délai pour pardonner un écart de souris
            InvalidateRect(h, null(), 0);
            0
        }
        WM_TIMER => {
            if wp == 1 {
                step(h);
            } else {
                KillTimer(h, 2);
                animate(h, 0.0);
            }
            0
        }
        WM_LBUTTONDOWN => {
            if ST.with_borrow(|s| s.t < 0.5) {
                // carré fermé : on peut le déplacer en le faisant glisser
                ReleaseCapture();
                SendMessageW(h, WM_NCLBUTTONDOWN, HTCAPTION as usize, 0);
            }
            0
        }
        WM_LBUTTONUP => {
            let hv = ST.with_borrow(|s| if s.t > 0.9 { s.hover } else { -1 });
            if hv >= 0 {
                let (_, _, target) = ITEMS[hv as usize];
                ShellExecuteW(null_mut(), w("open").as_ptr(), w(target).as_ptr(), null(), null(), SW_SHOWNORMAL);
                animate(h, 0.0);
            }
            0
        }
        // shortcut: clic droit = quitter, en attendant un vrai menu
        WM_RBUTTONUP => {
            DestroyWindow(h);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

fn main() {
    unsafe {
        let inst = GetModuleHandleW(null());
        let class = w("DESKTOUCH");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: inst,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        RegisterClassW(&wc);
        let h = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            class.as_ptr(), w("DESKTOUCH").as_ptr(), WS_POPUP,
            X, Y, SMALL as i32, SMALL as i32, null_mut(), null_mut(), inst, null());
        SetLayeredWindowAttributes(h, 0, IDLE_ALPHA as u8, LWA_ALPHA);
        ShowWindow(h, SW_SHOWNOACTIVATE);
        // juste au-dessus du bureau : visible sur le fond d'écran, caché par toute fenêtre ouverte
        let desktop = FindWindowW(w("Progman").as_ptr(), null());
        if !desktop.is_null() {
            SetWindowPos(h, desktop, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_test() {
        assert_eq!(hit(PAD, PAD), 0);
        assert_eq!(hit(PAD + TILE + GAP + 1, PAD), 1);
        assert_eq!(hit(PAD + TILE + 2, PAD), -1); // dans l'espace entre deux tuiles
        assert_eq!(hit(PAD, PAD + TILE + GAP), 3);
        assert_eq!(hit(0, 0), -1);
        assert_eq!(hit(PAD + 3 * (TILE + GAP), PAD), -1);
    }
}
