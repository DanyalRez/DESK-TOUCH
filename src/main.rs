#![windows_subsystem = "windows"]
//! DESKTOUCH : un petit carré discret sur le bureau qui s'ouvre en panneau de raccourcis au survol.
//! Zéro CPU au repos : le minuteur d'animation ne tourne que pendant une transition.
//! Les raccourcis sont les fichiers du dossier « Raccourcis » posé à côté de l'exe (.lnk, .url…).

use std::{cell::RefCell, ffi::c_void, mem::zeroed, os::windows::ffi::OsStrExt, path::PathBuf, ptr::null, time::SystemTime};
use windows_sys::{
    core::GUID,
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        System::{Com::CoInitializeEx, LibraryLoader::GetModuleHandleW, SystemInformation::GetTickCount64},
        UI::{
            Controls::{ImageList_GetIcon, HIMAGELIST, ILD_TRANSPARENT, MARGINS},
            Input::KeyboardAndMouse::*,
            Shell::{SHGetFileInfoW, SHGetImageList, ShellExecuteW, SHFILEINFOW, SHGFI_SYSICONINDEX, SHIL_EXTRALARGE},
            WindowsAndMessaging::*,
        },
    },
};

const WM_MOUSELEAVE: u32 = 0x02A3; // absent de windows-sys
const IID_IIMAGELIST: GUID = GUID::from_u128(0x46EB5926_582E_4017_9FDF_E8998DAA0950);
const X: i32 = 60;
const Y: i32 = 120;
const SMALL: f32 = 44.0;
const TILE: i32 = 92;
const GAP: i32 = 10;
const PAD: i32 = 14;
const MAX_COLS: usize = 4;
const ICON: i32 = 48;
const IDLE_ALPHA: f32 = 80.0;

struct Item {
    name: Vec<u16>,
    path: Vec<u16>, // terminé par 0, prêt pour ShellExecute
    icon: HBITMAP,  // 48×48 BGRA prémultiplié, ou null
}

struct State {
    t: f32,
    target: f32,
    last: u64,
    hover: i32,
    tracking: bool,
    icon_font: HFONT,
    text_font: HFONT,
    items: Vec<Item>,
    loaded: Option<SystemTime>,
}

thread_local! {
    static ST: RefCell<State> = const { RefCell::new(State {
        t: 0.0, target: 0.0, last: 0, hover: -1, tracking: false,
        icon_font: 0 as HFONT, text_font: 0 as HFONT, items: Vec::new(), loaded: None,
    }) };
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn rgb(v: f32) -> u32 {
    let c = v.clamp(0.0, 255.0) as u32;
    c | c << 8 | c << 16
}

fn null_hwnd() -> HWND {
    std::ptr::null_mut()
}

fn folder() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent().map(|p| p.join("Raccourcis")).unwrap_or_else(|| "Raccourcis".into())
}

/// Tuiles : les raccourcis, puis le « + » qui ouvre le dossier.
fn cols(tiles: usize) -> usize {
    tiles.clamp(1, MAX_COLS)
}

fn panel_size(tiles: usize) -> (f32, f32) {
    let (c, r) = (cols(tiles) as i32, tiles.div_ceil(MAX_COLS).max(1) as i32);
    ((PAD * 2 + TILE * c + GAP * (c - 1)) as f32, (PAD * 2 + TILE * r + GAP * (r - 1)) as f32)
}

/// Index de la tuile sous (x, y), ou -1.
fn hit(x: i32, y: i32, tiles: usize) -> i32 {
    let (cx, cy) = (x - PAD, y - PAD);
    if cx < 0 || cy < 0 || cx % (TILE + GAP) >= TILE || cy % (TILE + GAP) >= TILE {
        return -1;
    }
    let (col, row) = ((cx / (TILE + GAP)) as usize, (cy / (TILE + GAP)) as usize);
    let i = row * cols(tiles) + col;
    if col < cols(tiles) && i < tiles { i as i32 } else { -1 }
}

fn tile_rect(i: usize, tiles: usize) -> RECT {
    let (c, r) = ((i % cols(tiles)) as i32, (i / cols(tiles)) as i32);
    let (x, y) = (PAD + c * (TILE + GAP), PAD + r * (TILE + GAP));
    RECT { left: x, top: y, right: x + TILE, bottom: y + TILE }
}

/// Premier lancement : quelques raccourcis de départ pour ne pas ouvrir un panneau vide.
fn write_defaults(dir: &PathBuf) {
    let win = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let url = |name: &str, target: &str, icon: &str| {
        let ico = if icon.is_empty() { String::new() } else { format!("IconFile={icon}\r\nIconIndex=0\r\n") };
        let _ = std::fs::write(dir.join(format!("{name}.url")), format!("[InternetShortcut]\r\nURL={target}\r\n{ico}"));
    };
    let _ = std::fs::create_dir_all(dir);
    url("Fichiers", &format!("file:///{win}/explorer.exe").replace('\\', "/"), &format!(r"{win}\explorer.exe"));
    url("Web", "https://www.google.com", "");
    url("Calculatrice", &format!("file:///{win}/System32/calc.exe").replace('\\', "/"), &format!(r"{win}\System32\calc.exe"));
    url("Réglages", "ms-settings:", &format!(r"{win}\ImmersiveControlPanel\SystemSettings.exe"));
    let steam = r"C:\Program Files (x86)\Steam\steam.exe";
    if std::path::Path::new(steam).exists() {
        url("Steam", "steam://open/main", steam);
    }
}

/// Logo du fichier tel que l'Explorateur l'affiche, converti pour AlphaBlend.
unsafe fn file_icon(path: &[u16]) -> HBITMAP {
    let mut sfi: SHFILEINFOW = zeroed();
    if SHGetFileInfoW(path.as_ptr(), 0, &mut sfi, size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX) == 0 {
        return null_mut();
    }
    let mut list: *mut c_void = null_mut();
    if SHGetImageList(SHIL_EXTRALARGE as i32, &IID_IIMAGELIST, &mut list) != 0 {
        return null_mut();
    }
    let hicon = ImageList_GetIcon(list as HIMAGELIST, sfi.iIcon, ILD_TRANSPARENT);
    if hicon.is_null() {
        return null_mut();
    }
    let mut bmi: BITMAPINFO = zeroed();
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: ICON, biHeight: -ICON,
        biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, ..zeroed()
    };
    let mut bits: *mut c_void = null_mut();
    let dib = CreateDIBSection(null_mut(), &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    let dc = CreateCompatibleDC(null_mut());
    let old = SelectObject(dc, dib);
    // sur un fond à 0, DrawIconEx produit directement du BGRA prémultiplié
    DrawIconEx(dc, 0, 0, hicon, ICON, ICON, 0, null_mut(), DI_NORMAL);
    GdiFlush();
    let px = std::slice::from_raw_parts_mut(bits as *mut u32, (ICON * ICON) as usize);
    if px.iter().all(|p| p >> 24 == 0) {
        // vieille icône sans canal alpha : tout pixel non noir devient opaque
        for p in px.iter_mut().filter(|p| **p != 0) {
            *p |= 0xFF00_0000;
        }
    }
    SelectObject(dc, old);
    DeleteDC(dc);
    DestroyIcon(hicon);
    dib
}

/// Relit le dossier s'il a changé depuis la dernière ouverture (ajout, suppression, renommage).
unsafe fn reload_if_changed() {
    let dir = folder();
    if !dir.exists() {
        write_defaults(&dir);
    }
    let stamp = std::fs::metadata(&dir).and_then(|m| m.modified()).ok();
    if stamp.is_some() && ST.with_borrow(|s| s.loaded == stamp) {
        return;
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect())
        .unwrap_or_default();
    files.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    let items: Vec<Item> = files.iter().map(|p| {
        let path: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
        let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Item { name: stem.encode_utf16().collect(), icon: file_icon(&path), path }
    }).collect();
    let old = ST.with_borrow_mut(|s| {
        s.loaded = stamp;
        std::mem::replace(&mut s.items, items)
    });
    for it in old {
        if !it.icon.is_null() {
            DeleteObject(it.icon);
        }
    }
}

unsafe fn animate(h: HWND, target: f32) {
    if target > 0.5 && ST.with_borrow(|s| s.t == 0.0) {
        reload_if_changed();
    }
    ST.with_borrow_mut(|s| {
        s.target = target;
        s.last = GetTickCount64();
    });
    SetTimer(h, 1, 8, None);
}

unsafe fn step(h: HWND) {
    let (t, done, tiles) = ST.with_borrow_mut(|s| {
        let now = GetTickCount64();
        let dt = (now - s.last) as f32;
        s.last = now;
        // approche exponentielle : indépendante du nombre d'images, ralentit en douceur
        s.t += (s.target - s.t) * (1.0 - (-dt / 70.0).exp());
        if (s.target - s.t).abs() < 0.002 {
            s.t = s.target;
        }
        (s.t, s.t == s.target, s.items.len() + 1)
    });
    if done {
        KillTimer(h, 1);
    }
    let (bw, bh) = panel_size(tiles);
    let size = |a: f32, b: f32| (a + (b - a) * t).round() as i32;
    SetWindowPos(h, null_hwnd(), 0, 0, size(SMALL, bw), size(SMALL, bh),
        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
    SetLayeredWindowAttributes(h, 0, (IDLE_ALPHA + (255.0 - IDLE_ALPHA) * t * 2.0).min(255.0) as u8, LWA_ALPHA);
    InvalidateRect(h, null(), 0);
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

    ST.with_borrow(|s| {
        // les couleurs s'additionnent au verre : baisser l'intensité revient à faire un fondu
        let open = ((s.t - 0.45) / 0.55).clamp(0.0, 1.0);
        let closed = (1.0 - s.t * 2.5).clamp(0.0, 1.0);

        if closed > 0.0 {
            let b = CreateSolidBrush(rgb(210.0 * closed));
            let old_b = SelectObject(mem, b);
            for (dx, dy) in [(13, 13), (23, 13), (13, 23), (23, 23)] {
                RoundRect(mem, dx, dy, dx + 9, dy + 9, 4, 4);
            }
            SelectObject(mem, old_b);
            DeleteObject(b);
        }
        if open == 0.0 {
            return;
        }

        let tiles = s.items.len() + 1;
        let src = CreateCompatibleDC(mem);
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0,
            SourceConstantAlpha: (255.0 * open) as u8, AlphaFormat: AC_SRC_ALPHA as u8 };
        for i in 0..tiles {
            let r = tile_rect(i, tiles);
            let b = CreateSolidBrush(rgb(if s.hover == i as i32 { 62.0 } else { 26.0 } * open));
            let old_b = SelectObject(mem, b);
            RoundRect(mem, r.left, r.top, r.right + 1, r.bottom + 1, 22, 22);
            SelectObject(mem, old_b);
            DeleteObject(b);

            let (ix, iy) = (r.left + (TILE - ICON) / 2, r.top + 12);
            let item = s.items.get(i);
            match item {
                Some(it) if !it.icon.is_null() => {
                    let old = SelectObject(src, it.icon);
                    AlphaBlend(mem, ix, iy, ICON, ICON, src, 0, 0, ICON, ICON, blend);
                    SelectObject(src, old);
                }
                _ => {
                    // pas de logo : première lettre du nom ; dernière tuile : « + » (glyphe Fluent)
                    let glyph = item.map_or(0xE710, |it| it.name.first().copied().unwrap_or(b'?' as u16));
                    SelectObject(mem, if item.is_some() { s.text_font } else { s.icon_font });
                    SetTextColor(mem, rgb(230.0 * open));
                    let mut ri = RECT { left: ix, top: iy, right: ix + ICON, bottom: iy + ICON };
                    DrawTextW(mem, &glyph, 1, &mut ri, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                }
            }

            let label = item.map_or(w("Ajouter")[..7].to_vec(), |it| it.name.clone());
            SelectObject(mem, s.text_font);
            SetTextColor(mem, rgb(205.0 * open));
            let mut rt = RECT { left: r.left + 6, right: r.right - 6, top: r.top + 64, bottom: r.bottom - 6 };
            DrawTextW(mem, label.as_ptr(), label.len() as i32, &mut rt,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        }
        DeleteDC(src);
    });

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
                let hv = if s.t > 0.9 { hit(x, y, s.items.len() + 1) } else { -1 };
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
            let target = ST.with_borrow(|s| {
                if s.t <= 0.9 || s.hover < 0 {
                    return None;
                }
                Some(s.items.get(s.hover as usize).map_or_else(
                    || folder().as_os_str().encode_wide().chain(Some(0)).collect(),
                    |it| it.path.clone()))
            });
            if let Some(path) = target {
                ShellExecuteW(null_hwnd(), w("open").as_ptr(), path.as_ptr(), null(), null(), SW_SHOWNORMAL);
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
        CoInitializeEx(null(), 2); // COINIT_APARTMENTTHREADED, requis par SHGetFileInfo
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
            X, Y, SMALL as i32, SMALL as i32, null_hwnd(), null_mut(), inst, null());
        SetLayeredWindowAttributes(h, 0, IDLE_ALPHA as u8, LWA_ALPHA);
        ShowWindow(h, SW_SHOWNOACTIVATE);
        // juste au-dessus du bureau : visible sur le fond d'écran, caché par toute fenêtre ouverte
        let desktop = FindWindowW(w("Progman").as_ptr(), null());
        if !desktop.is_null() {
            SetWindowPos(h, desktop, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_hwnd(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn null_mut<T>() -> *mut T {
    std::ptr::null_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_test() {
        let n = 7; // 2 rangées de 4, la dernière incomplète
        assert_eq!(hit(PAD, PAD, n), 0);
        assert_eq!(hit(PAD + TILE + GAP + 1, PAD, n), 1);
        assert_eq!(hit(PAD + TILE + 2, PAD, n), -1); // dans l'espace entre deux tuiles
        assert_eq!(hit(PAD, PAD + TILE + GAP, n), 4);
        assert_eq!(hit(PAD + 3 * (TILE + GAP), PAD + TILE + GAP, n), -1); // 8e case vide
        assert_eq!(hit(0, 0, n), -1);
        assert_eq!(hit(PAD + 4 * (TILE + GAP), PAD, n), -1);
        assert_eq!(hit(PAD + TILE + GAP, PAD, 1), -1); // une seule tuile
    }

    #[test]
    fn panel_grows_with_tiles() {
        assert!(panel_size(4).0 > panel_size(2).0);
        assert_eq!(panel_size(5).0, panel_size(4).0);
        assert!(panel_size(5).1 > panel_size(4).1);
    }
}
