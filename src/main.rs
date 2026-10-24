#![windows_subsystem = "windows"]
//! DESKTOUCH : un petit carré de verre sur le bureau ; au survol, un panneau de raccourcis en sort
//! comme une goutte. Visible seulement quand le bureau est au premier plan.
//! Zéro CPU au repos : on ne calcule des images que pendant une animation, cadencées sur l'écran.

mod render;
mod shell;

use render::{smoothstep, Back, Frame, Layer, Look, Pill, Rgb, Rr, Spring};
use shell::{Cell, Config, Item, Text};
use std::{
    cell::{Cell as StdCell, RefCell},
    mem::zeroed,
    ptr::null,
    sync::atomic::{AtomicIsize, Ordering},
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::DwmFlush, Gdi::*},
    System::{Com::CoInitializeEx, LibraryLoader::GetModuleHandleW},
    UI::{
        Accessibility::{SetWinEventHook, HWINEVENTHOOK},
        HiDpi::{GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
        Input::KeyboardAndMouse::*,
        Shell::ShellExecuteW,
        WindowsAndMessaging::*,
    },
};

const WM_MOUSELEAVE: u32 = 0x02A3; // absent de windows-sys
const WM_DPICHANGED: u32 = 0x02E0;
const WM_TRAY: u32 = WM_APP + 1;
const WM_FOREGROUND: u32 = WM_APP + 2;
const T_OPEN: usize = 1;
const T_CLOSE: usize = 2;
const ID_SETTINGS: usize = 1;
const ID_ADD: usize = 2;
const ID_FOLDER: usize = 3;
const ID_QUIT: usize = 4;
const ID_REMOVE: usize = 5;
// tailles en pixels logiques (× échelle de l'écran)
const SQ: f32 = 46.0;
const SQ_R: f32 = 15.0;
const PANEL_R: f32 = 28.0;
const GAP: f32 = 8.0; // entre le carré et le panneau ouvert
const PAD: f32 = 12.0;
const CELL_W: f32 = 88.0;
const CELL_H: f32 = 92.0;
const ICON: f32 = 46.0;
const MARGIN: f32 = 30.0; // place pour l'ombre et le rebond du ressort
const MAX_COLS: usize = 4;
const PREFS_W: f32 = 340.0;
const PREFS_H: f32 = 246.0;

static MAIN: AtomicIsize = AtomicIsize::new(0);
thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static TASKBAR_CREATED: StdCell<u32> = const { StdCell::new(0) };
}

fn app<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with_borrow_mut(|a| f(a.as_mut().expect("app")))
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn lparam_xy(lp: LPARAM) -> (f32, f32) {
    ((lp & 0xFFFF) as i16 as f32, ((lp >> 16) & 0xFFFF) as i16 as f32)
}

/// Index de la case sous (x, y) relatif à la grille, ou -1.
fn hit(x: f32, y: f32, cols: usize, tiles: usize, cw: f32, ch: f32) -> i32 {
    if x < 0.0 || y < 0.0 {
        return -1;
    }
    let (c, r) = ((x / cw) as usize, (y / ch) as usize);
    let i = r * cols + c;
    if c < cols && i < tiles { i as i32 } else { -1 }
}

/// Apparence selon le thème ; en Liquid Glass, le texte s'adapte au fond.
fn look(theme: usize, light_back: bool) -> Look {
    const DARK_INK: Rgb = [0.08, 0.08, 0.10];
    match theme {
        1 => Look { refraction: 0.0, tint: [0.07, 0.07, 0.09], tint_amount: 0.62, ink: [1.0; 3] },
        2 => Look { refraction: 0.0, tint: [0.97, 0.97, 0.98], tint_amount: 0.68, ink: DARK_INK },
        _ if light_back => Look { refraction: 1.0, tint: [1.0; 3], tint_amount: 0.22, ink: DARK_INK },
        _ => Look { refraction: 1.0, tint: [1.0; 3], tint_amount: 0.06, ink: [1.0; 3] },
    }
}

/// Couleur des éléments posés sur le verre : blanc sur verre sombre, noir sur verre clair.
fn accent(l: &Look) -> Rgb {
    if l.ink[0] > 0.5 { [1.0; 3] } else { [0.0; 3] }
}

/// Fenêtre en couches dessinée par notre moteur (pixels à nous, alpha par pixel).
struct Surface {
    hwnd: HWND,
    win: RECT,
    dc: HDC,
    dib: HBITMAP,
    bits: *mut u32,
    back: Back,
    layer: Layer,
    light_back: bool,
}

impl Surface {
    unsafe fn new(hwnd: HWND) -> Self {
        Surface { hwnd, win: zeroed(), dc: CreateCompatibleDC(null_mut()), dib: null_mut(), bits: null_mut(),
            back: Back::empty(), layer: Layer::empty(), light_back: false }
    }

    fn size(&self) -> (usize, usize) {
        ((self.win.right - self.win.left) as usize, (self.win.bottom - self.win.top) as usize)
    }

    unsafe fn place(&mut self, r: RECT) {
        let old = self.size();
        self.win = r;
        if old != self.size() || self.dib.is_null() {
            if !self.dib.is_null() {
                DeleteObject(self.dib);
            }
            let (ww, wh) = self.size();
            let (bmp, bits) = shell::dib(ww as i32, wh as i32);
            self.dib = bmp;
            self.bits = bits;
            SelectObject(self.dc, bmp);
        }
    }

    /// Nouvelle photo du fond ; `probe` : zone dont la luminosité décide du texte clair ou foncé.
    unsafe fn capture(&mut self, blur: usize, probe: Rr) {
        let (ww, wh) = self.size();
        self.back = shell::capture(self.hwnd, self.win.left, self.win.top, ww, wh);
        self.back.frost(blur);
        self.light_back = self.back.luminance(probe.x as usize, probe.y as usize, probe.w as usize, probe.h as usize) > 0.6;
    }

    unsafe fn present(&self, f: &Frame) {
        let (ww, wh) = self.size();
        render::render(f, std::slice::from_raw_parts_mut(self.bits, ww * wh), ww);
        GdiFlush();
        let pos = POINT { x: self.win.left, y: self.win.top };
        let size = SIZE { cx: ww as i32, cy: wh as i32 };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        UpdateLayeredWindow(self.hwnd, null_mut(), &pos, &size, self.dc, &src, 0, &blend, ULW_ALPHA);
    }
}

struct App {
    cfg: Config,
    scale: f32,
    // carré + panneau
    main: Surface,
    items: Vec<Item>,
    loaded: Option<std::time::SystemTime>,
    sq_x: i32, // coin haut-gauche du carré, à l'écran
    sq_y: i32,
    right: bool, // le panneau s'ouvre vers la droite / vers le bas
    down: bool,
    open: Spring,
    vis: Spring,
    bx: Spring,
    by: Spring,
    blob: Spring,
    press: Spring,
    hover: i32,
    tracking: bool,
    desktop: bool,
    // réglages
    prefs: Surface,
    prefs_shown: bool,
    p_open: Spring,
    p_seg: Spring,
    p_knob: Spring,
    p_tog: Spring,
    dragging: bool,
    // boucle
    dirty_main: bool,
    dirty_prefs: bool,
    animating: bool,
    last: Instant,
}

impl App {
    fn px(&self, v: f32) -> f32 {
        v * self.scale
    }
    fn look(&self, s: &Surface) -> Look {
        look(self.cfg.theme, s.light_back)
    }

    // ---------- géométrie du carré et du panneau (coordonnées de la fenêtre) ----------

    fn tiles(&self) -> usize {
        self.items.len() + 1 // + « Ajouter »
    }
    fn cols(&self) -> usize {
        self.tiles().clamp(1, MAX_COLS)
    }
    fn panel_size(&self) -> (f32, f32) {
        let rows = self.tiles().div_ceil(MAX_COLS);
        (self.px(PAD * 2.0 + CELL_W * self.cols() as f32).round(), self.px(PAD * 2.0 + CELL_H * rows as f32).round())
    }

    fn square(&self) -> Rr {
        let (ww, wh) = self.main.size();
        let (sq, m) = (self.px(SQ), self.px(MARGIN));
        Rr { x: if self.right { m } else { ww as f32 - m - sq }, y: if self.down { m } else { wh as f32 - m - sq },
            w: sq, h: sq, r: self.px(SQ_R) }
    }

    /// Panneau ouvert : il part du coin du carré, en diagonale (comme sur le croquis).
    fn panel(&self) -> Rr {
        let s = self.square();
        let (pw, ph) = self.panel_size();
        let g = self.px(GAP);
        Rr { x: if self.right { s.x + s.w + g } else { s.x - g - pw }, y: if self.down { s.y + s.h + g } else { s.y - g - ph },
            w: pw, h: ph, r: self.px(PANEL_R) }
    }

    /// Le panneau pendant l'animation : il sort du carré en grandissant.
    fn panel_at(&self, o: f32) -> Option<Rr> {
        (o > 0.002).then(|| Rr::lerp(self.square(), self.panel(), o))
    }

    fn cells(&self) -> Vec<Cell> {
        let cols = self.cols();
        (0..self.tiles()).map(|i| Cell {
            x: self.px(PAD + (i % cols) as f32 * CELL_W).round() as i32,
            y: self.px(PAD + (i / cols) as f32 * CELL_H).round() as i32,
            w: self.px(CELL_W).round() as i32,
            h: self.px(CELL_H).round() as i32,
        }).collect()
    }

    /// Recalcule la taille et la place de la fenêtre (le carré, lui, ne bouge pas).
    unsafe fn relayout(&mut self) {
        let (pw, ph) = self.panel_size();
        let (sq, m, g) = (self.px(SQ), self.px(MARGIN), self.px(GAP));
        let mon = MonitorFromPoint(POINT { x: self.sq_x, y: self.sq_y }, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        let wk = mi.rcWork;
        let (x, y) = (self.sq_x as f32, self.sq_y as f32);
        // pas de place à droite / en bas : on ouvre de l'autre côté
        self.right = x + sq + g + pw <= wk.right as f32 || x - g - pw < wk.left as f32;
        self.down = y + sq + g + ph <= wk.bottom as f32 || y - g - ph < wk.top as f32;
        let (ww, wh) = ((sq + g + pw + 2.0 * m).ceil() as i32, (sq + g + ph + 2.0 * m).ceil() as i32);
        let sx = if self.right { m as i32 } else { ww - m as i32 - sq as i32 };
        let sy = if self.down { m as i32 } else { wh - m as i32 - sq as i32 };
        let (left, top) = (self.sq_x - sx, self.sq_y - sy);
        self.main.place(RECT { left, top, right: left + ww, bottom: top + wh });
    }

    unsafe fn recapture(&mut self) {
        let probe = self.panel();
        self.main.capture(self.px(4.0).round() as usize, probe);
        self.rebuild_main_layer();
    }

    unsafe fn rebuild_main_layer(&mut self) {
        let (pw, ph) = self.panel_size();
        let cells = self.cells();
        let ink = self.look(&self.main).ink;
        self.main.layer = shell::tiles_layer(&self.items, &cells, pw as usize, ph as usize,
            self.px(ICON).round() as usize, self.scale, ink);
        self.dirty_main = true;
    }

    unsafe fn reload(&mut self, force: bool) {
        if let Some((items, stamp)) = shell::load_items(if force { None } else { self.loaded }) {
            self.items = items;
            self.loaded = stamp;
            self.relayout();
            self.rebuild_main_layer();
        }
    }

    fn vis_target(&self) -> f32 {
        if !self.desktop {
            0.0
        } else if self.tracking || self.open.target > 0.5 {
            1.0
        } else {
            self.cfg.opacity
        }
    }

    unsafe fn draw_main(&self) {
        let o = self.open.x;
        let oc = o.clamp(0.0, 1.0);
        let content = smoothstep(0.55, 0.95, o);
        let panel = self.panel();
        let look = self.look(&self.main);
        let inset = self.px(4.0 + 3.0 * self.press.x);
        let blob = self.blob.x.clamp(0.0, 1.0) * content;
        let pills = [Pill {
            rr: Rr { x: self.bx.x + inset, y: self.by.x + inset, w: self.px(CELL_W) - 2.0 * inset,
                h: self.px(CELL_H) - 2.0 * inset, r: self.px(20.0) },
            color: accent(&look), amount: 0.15 * blob, rim: 0.55 * blob,
        }];
        let f = Frame {
            back: &self.main.back,
            layer: &self.main.layer,
            layer_pos: (panel.x.round() as i32, (panel.y + (1.0 - content) * self.px(8.0)).round() as i32),
            layer_alpha: content,
            a: self.square(),
            b: self.panel_at(o),
            // fusion forte au départ (la goutte s'étire), puis les deux formes se détachent
            smooth: self.px(18.0) * smoothstep(0.0, 0.12, o) * lerp(1.0, 0.3, oc),
            refr_height: self.px(lerp(14.0, 22.0, oc)),
            refr_amount: self.px(lerp(20.0, 40.0, oc)),
            glyph: true,
            pills: &pills,
            alpha: self.vis.x.clamp(0.0, 1.0),
            scale: self.scale,
            look,
        };
        self.main.present(&f);
    }

    fn kick(&mut self) {
        if !self.animating {
            self.animating = true;
            self.last = Instant::now();
        }
    }

    unsafe fn begin_open(&mut self) {
        if !self.desktop {
            return;
        }
        self.reload(false);
        self.recapture();
        self.open.damping = 0.74; // léger rebond à l'ouverture
        self.open.target = 1.0;
        self.vis.target = self.vis_target();
        self.kick();
    }

    fn close(&mut self) {
        self.open.damping = 1.0;
        self.open.target = 0.0;
        self.blob.target = 0.0;
        self.hover = -1;
        self.vis.target = self.vis_target();
        self.kick();
    }

    fn set_desktop(&mut self, d: bool) {
        if d == self.desktop {
            return;
        }
        self.desktop = d;
        if !d {
            self.close();
        }
        self.vis.target = self.vis_target();
        self.kick();
    }

    fn set_hover(&mut self, mx: f32, my: f32) {
        let p = self.panel();
        let hv = if self.open.x > 0.85 && self.open.target == 1.0 {
            hit(mx - p.x - self.px(PAD), my - p.y - self.px(PAD), self.cols(), self.tiles(), self.px(CELL_W), self.px(CELL_H))
        } else {
            -1
        };
        if hv == self.hover {
            return;
        }
        self.hover = hv;
        if hv >= 0 {
            let c = &self.cells()[hv as usize];
            let (tx, ty) = (p.x + c.x as f32, p.y + c.y as f32);
            if self.blob.x < 0.05 {
                // la bulle apparaît sur place puis glisse d'une case à l'autre
                self.bx.snap(tx);
                self.by.snap(ty);
            }
            self.bx.target = tx;
            self.by.target = ty;
            self.blob.target = 1.0;
        } else {
            self.blob.target = 0.0;
        }
        self.kick();
    }

    fn over_shape(&self, x: f32, y: f32) -> bool {
        self.square().contains(x, y) || self.panel_at(self.open.x).is_some_and(|p| p.contains(x, y))
    }

    // ---------- réglages ----------

    fn prefs_panel(&self) -> Rr {
        let m = self.px(MARGIN);
        Rr { x: m, y: m, w: self.px(PREFS_W), h: self.px(PREFS_H), r: self.px(PANEL_R) }
    }

    unsafe fn show_prefs(&mut self) {
        if self.prefs_shown {
            return;
        }
        let mut c: POINT = zeroed();
        GetCursorPos(&mut c);
        let mon = MonitorFromPoint(c, MONITOR_DEFAULTTOPRIMARY);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        let p = self.prefs_panel();
        let (ww, wh) = ((p.w + 2.0 * p.x) as i32, (p.h + 2.0 * p.y) as i32);
        // en bas à droite, au-dessus de la zone de notification
        let (left, top) = (mi.rcWork.right - ww + p.x as i32 - self.px(12.0) as i32, mi.rcWork.bottom - wh + p.y as i32 - self.px(12.0) as i32);
        self.prefs.place(RECT { left, top, right: left + ww, bottom: top + wh });
        self.prefs.capture(self.px(10.0).round() as usize, p);
        self.p_seg.snap(self.cfg.theme as f32);
        self.p_knob.snap((self.cfg.opacity - 0.15) / 0.85);
        self.p_tog.snap(self.cfg.startup as u8 as f32);
        self.p_open.snap(0.0);
        self.rebuild_prefs_layer();
        self.draw_prefs();
        // afficher sans activer puis demander le premier plan : si Windows refuse, la fenêtre reste ouverte
        // (Échap ou un nouveau clic sur l'icône la referment) au lieu de clignoter et disparaître
        ShowWindow(self.prefs.hwnd, SW_SHOWNA);
        SetForegroundWindow(self.prefs.hwnd);
        self.prefs_shown = true;
        self.p_open.target = 1.0;
        self.kick();
    }

    fn hide_prefs(&mut self) {
        self.p_open.target = 0.0;
        self.kick();
    }

    unsafe fn rebuild_prefs_layer(&mut self) {
        let p = self.prefs_panel();
        let s = self.scale;
        let r = |x: f32, y: f32, w: f32, h: f32| RECT {
            left: (x * s) as i32, top: (y * s) as i32, right: ((x + w) * s) as i32, bottom: ((y + h) * s) as i32,
        };
        let pct = format!("{} %", (self.cfg.opacity * 100.0).round());
        let mut texts = vec![
            Text { s: "Réglages", r: r(22.0, 16.0, 296.0, 26.0), size: 17.0 * s, weight: 600, flags: DT_LEFT | DT_VCENTER, level: 1.0, glyph: false },
            Text { s: "Thème", r: r(22.0, 52.0, 200.0, 20.0), size: 12.5 * s, weight: 500, flags: DT_LEFT | DT_VCENTER, level: 0.62, glyph: false },
            Text { s: "Opacité du logo", r: r(22.0, 130.0, 200.0, 20.0), size: 12.5 * s, weight: 500, flags: DT_LEFT | DT_VCENTER, level: 0.62, glyph: false },
            Text { s: &pct, r: r(220.0, 130.0, 100.0, 20.0), size: 12.5 * s, weight: 600, flags: DT_RIGHT | DT_VCENTER, level: 0.9, glyph: false },
            Text { s: "Lancer au démarrage", r: r(22.0, 198.0, 230.0, 28.0), size: 13.5 * s, weight: 500, flags: DT_LEFT | DT_VCENTER, level: 1.0, glyph: false },
        ];
        for (i, name) in shell::THEMES.iter().enumerate() {
            texts.push(Text { s: name, r: r(20.0 + i as f32 * 100.0, 78.0, 100.0, 38.0), size: 12.5 * s, weight: 600,
                flags: DT_CENTER | DT_VCENTER, level: 1.0, glyph: false });
        }
        let ink = self.look(&self.prefs).ink;
        self.prefs.layer = shell::text_layer(p.w as usize, p.h as usize, &texts, ink);
        self.dirty_prefs = true;
    }

    unsafe fn draw_prefs(&self) {
        let o = self.p_open.x;
        let p = self.prefs_panel();
        // entre en grandissant un peu depuis le coin bas-droit, côté zone de notification
        let k = lerp(0.94, 1.0, o.clamp(0.0, 1.0));
        let shape = Rr { x: p.x + p.w * (1.0 - k), y: p.y + p.h * (1.0 - k), w: p.w * k, h: p.h * k, r: p.r };
        let content = smoothstep(0.4, 1.0, o);
        let look = self.look(&self.prefs);
        let ac = accent(&look);
        let s = self.scale;
        let at = |x: f32, y: f32, w: f32, h: f32, r: f32| Rr { x: p.x + x * s, y: p.y + y * s, w: w * s, h: h * s, r: r * s };
        let knob_x = 20.0 + self.p_knob.x.clamp(0.0, 1.0) * (300.0 - 22.0);
        let tog = self.p_tog.x;
        let pills = [
            Pill { rr: at(20.0, 78.0, 300.0, 38.0, 19.0), color: ac, amount: 0.08 * content, rim: 0.0 },
            Pill { rr: at(23.0 + self.p_seg.x * 98.0, 81.0, 98.0, 32.0, 16.0), color: ac, amount: 0.2 * content, rim: 0.7 * content },
            Pill { rr: at(20.0, 164.0, 300.0, 6.0, 3.0), color: ac, amount: 0.16 * content, rim: 0.0 },
            Pill { rr: at(20.0, 164.0, knob_x - 20.0 + 11.0, 6.0, 3.0), color: ac, amount: 0.6 * content, rim: 0.0 },
            Pill { rr: at(knob_x, 156.0, 22.0, 22.0, 11.0), color: [1.0; 3], amount: content, rim: 0.6 * content },
            Pill { rr: at(272.0, 198.0, 48.0, 28.0, 14.0), color: ac, amount: lerp(0.14, 0.6, tog) * content, rim: 0.3 * content },
            Pill { rr: at(275.0 + tog * 20.0, 201.0, 22.0, 22.0, 11.0), color: [1.0; 3], amount: content, rim: 0.6 * content },
        ];
        let f = Frame {
            back: &self.prefs.back,
            layer: &self.prefs.layer,
            layer_pos: (p.x as i32, p.y as i32),
            layer_alpha: content,
            a: shape,
            b: None,
            smooth: 0.0,
            refr_height: self.px(22.0),
            refr_amount: self.px(36.0),
            glyph: false,
            pills: &pills,
            alpha: o.clamp(0.0, 1.0),
            scale: s,
            look,
        };
        self.prefs.present(&f);
    }

    unsafe fn set_theme(&mut self, t: usize) {
        self.cfg.theme = t;
        self.p_seg.target = t as f32;
        shell::save_config(&self.cfg);
        self.rebuild_main_layer();
        self.rebuild_prefs_layer();
        self.kick();
    }

    unsafe fn set_opacity_from(&mut self, lx: f32) {
        let t = ((lx - 31.0) / 278.0).clamp(0.0, 1.0);
        self.cfg.opacity = 0.15 + t * 0.85;
        self.p_knob.target = t;
        self.vis.target = self.vis_target();
        self.rebuild_prefs_layer();
        self.kick();
    }

    unsafe fn toggle_startup(&mut self) {
        self.cfg.startup = !self.cfg.startup;
        shell::set_startup(self.cfg.startup);
        shell::save_config(&self.cfg);
        self.p_tog.target = self.cfg.startup as u8 as f32;
        self.kick();
    }

    // ---------- boucle d'animation ----------

    fn main_springs(&mut self) -> [&mut Spring; 6] {
        [&mut self.open, &mut self.vis, &mut self.bx, &mut self.by, &mut self.blob, &mut self.press]
    }
    fn prefs_springs(&mut self) -> [&mut Spring; 4] {
        [&mut self.p_open, &mut self.p_seg, &mut self.p_knob, &mut self.p_tog]
    }

    /// Une image d'animation pour chaque fenêtre qui bouge ; faux quand tout est au repos.
    unsafe fn tick(&mut self) -> bool {
        let now = Instant::now();
        let dt = (now - self.last).as_secs_f32().min(1.0 / 30.0);
        self.last = now;
        let main_moving = !self.main_springs().iter().all(|s| s.rest());
        let prefs_moving = !self.prefs_springs().iter().all(|s| s.rest());
        self.main_springs().into_iter().for_each(|s| s.step(dt));
        self.prefs_springs().into_iter().for_each(|s| s.step(dt));
        if main_moving || self.dirty_main {
            self.draw_main();
            self.dirty_main = false;
        }
        if self.prefs_shown && (prefs_moving || self.dirty_prefs) {
            self.draw_prefs();
            self.dirty_prefs = false;
            if self.p_open.target == 0.0 && self.p_open.rest() {
                ShowWindow(self.prefs.hwnd, SW_HIDE);
                self.prefs_shown = false;
            }
        }
        self.animating = !self.main_springs().iter().all(|s| s.rest()) || !self.prefs_springs().iter().all(|s| s.rest());
        self.animating
    }
}

// ---------- actions qui ouvrent une fenêtre modale (à appeler hors de tout emprunt de l'app) ----------

unsafe fn main_menu(h: HWND) {
    let id = shell::menu(h, &[(ID_SETTINGS, "Réglages"), (ID_ADD, "Ajouter un raccourci…"),
        (ID_FOLDER, "Ouvrir le dossier des raccourcis"), (0, ""), (ID_QUIT, "Quitter DESKTOUCH")]);
    match id {
        ID_SETTINGS => app(|a| a.show_prefs()),
        ID_ADD => add_flow(h),
        ID_FOLDER => {
            ShellExecuteW(null_mut(), shell::w("open").as_ptr(), shell::wide_path(&shell::folder()).as_ptr(), null(), null(), SW_SHOWNORMAL);
        }
        ID_QUIT => {
            DestroyWindow(h);
        }
        _ => {}
    }
}

unsafe fn add_flow(h: HWND) {
    app(|a| a.close());
    if let Some(p) = shell::pick_file(h) {
        if shell::add_shortcut(&p) {
            app(|a| a.reload(true));
        }
    }
}

unsafe extern "system" fn win_event(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    PostMessageW(MAIN.load(Ordering::Relaxed) as HWND, WM_FOREGROUND, 0, 0);
}

unsafe extern "system" fn main_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // pas encore créée, ou message renvoyé par Windows pendant qu'on la modifie : traitement par défaut
    if APP.with(|c| c.try_borrow().map_or(true, |a| a.is_none())) {
        return DefWindowProcW(h, msg, wp, lp);
    }
    match msg {
        // ne jamais voler le focus ni passer devant les autres fenêtres
        WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
        WM_MOUSEMOVE => {
            let (x, y) = lparam_xy(lp);
            let enter = app(|a| !std::mem::replace(&mut a.tracking, true));
            if enter {
                let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                TrackMouseEvent(&mut tme);
                KillTimer(h, T_CLOSE);
                let closed = app(|a| {
                    a.vis.target = a.vis_target();
                    a.kick();
                    a.open.target < 0.5
                });
                if closed {
                    SetTimer(h, T_OPEN, 140, None); // court temps d'arrêt : on peut traverser ou attraper le carré
                }
            }
            app(|a| a.set_hover(x, y));
            0
        }
        WM_MOUSELEAVE => {
            // changer l'exclusion de capture déclenche une fausse sortie : on vérifie la souris
            let mut c: POINT = zeroed();
            GetCursorPos(&mut c);
            let inside = app(|a| a.over_shape((c.x - a.main.win.left) as f32, (c.y - a.main.win.top) as f32));
            if inside {
                let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                TrackMouseEvent(&mut tme);
                return 0;
            }
            app(|a| {
                a.tracking = false;
                a.hover = -1;
                a.blob.target = 0.0;
                a.vis.target = a.vis_target();
                a.kick();
            });
            KillTimer(h, T_OPEN);
            SetTimer(h, T_CLOSE, 300, None); // pardonne un écart de souris
            0
        }
        WM_TIMER => {
            KillTimer(h, wp);
            if wp == T_OPEN {
                app(|a| a.begin_open());
            } else {
                app(|a| a.close());
            }
            0
        }
        WM_LBUTTONDOWN => {
            let (x, y) = lparam_xy(lp);
            // sur le carré : fermé, on le déplace (ou on l'ouvre d'un clic) ; ouvert, un clic le referme
            let on_square = app(|a| {
                if !a.square().contains(x, y) {
                    a.press.target = 1.0;
                    a.kick();
                    return None;
                }
                if a.open.x > 0.3 {
                    a.close();
                    return None;
                }
                a.open.snap(0.0);
                a.draw_main();
                Some(a.main.win)
            });
            if let Some(before) = on_square {
                KillTimer(h, T_OPEN);
                ReleaseCapture();
                SendMessageW(h, WM_NCLBUTTONDOWN, HTCAPTION as usize, 0);
                let mut after: RECT = zeroed();
                GetWindowRect(h, &mut after);
                if (after.left, after.top) == (before.left, before.top) {
                    app(|a| a.begin_open()); // simple clic
                }
            }
            0
        }
        WM_LBUTTONUP => {
            enum Click { Launch(Vec<u16>), Add, Nothing }
            let click = app(|a| {
                a.press.target = 0.0;
                a.kick();
                if a.open.x <= 0.85 || a.hover < 0 {
                    return Click::Nothing;
                }
                match a.items.get(a.hover as usize) {
                    Some(it) => {
                        let p = it.path.clone();
                        a.close();
                        Click::Launch(p)
                    }
                    None => Click::Add,
                }
            });
            match click {
                Click::Launch(path) => {
                    ShellExecuteW(null_mut(), shell::w("open").as_ptr(), path.as_ptr(), null(), null(), SW_SHOWNORMAL);
                }
                Click::Add => add_flow(h),
                Click::Nothing => {}
            }
            0
        }
        WM_RBUTTONUP => {
            let target = app(|a| {
                if a.open.x <= 0.85 || a.hover < 0 {
                    return None;
                }
                a.items.get(a.hover as usize).map(|it| (it.name.clone(), it.path.clone()))
            });
            match target {
                Some((name, path)) => {
                    if shell::menu(h, &[(ID_REMOVE, &format!("Retirer « {name} »"))]) == ID_REMOVE && shell::remove_shortcut(&path) {
                        app(|a| a.reload(true));
                    }
                }
                None => main_menu(h),
            }
            0
        }
        // fin d'un glisser-déplacer du carré
        WM_EXITSIZEMOVE => {
            let mut r: RECT = zeroed();
            GetWindowRect(h, &mut r);
            app(|a| {
                let (dx, dy) = (r.left - a.main.win.left, r.top - a.main.win.top);
                if (dx, dy) == (0, 0) {
                    return;
                }
                a.sq_x += dx;
                a.sq_y += dy;
                shell::save_pos(a.sq_x, a.sq_y);
                a.relayout();
                a.recapture();
                a.draw_main();
            });
            0
        }
        WM_FOREGROUND => {
            if let Some(d) = shell::foreground_is_desktop() {
                app(|a| a.set_desktop(d));
            }
            0
        }
        WM_TRAY => {
            match lp as u32 {
                WM_LBUTTONUP => app(|a| if a.prefs_shown { a.hide_prefs() } else { a.show_prefs() }),
                WM_RBUTTONUP => main_menu(h),
                _ => {}
            }
            0
        }
        WM_DPICHANGED => {
            app(|a| {
                a.scale = (wp & 0xFFFF) as f32 / 96.0;
                a.relayout();
                a.recapture();
                a.draw_main();
            });
            0
        }
        // fond d'écran ou écrans changés : nouvelle photo du fond
        WM_SETTINGCHANGE | WM_DISPLAYCHANGE => {
            app(|a| {
                if a.open.x == 0.0 {
                    a.relayout();
                    a.recapture();
                    a.draw_main();
                }
            });
            0
        }
        WM_DESTROY => {
            shell::tray(h, WM_TRAY, false);
            PostQuitMessage(0);
            0
        }
        // l'Explorateur a redémarré : remettre l'icône de notification
        m if m == TASKBAR_CREATED.get() => {
            shell::tray(h, WM_TRAY, true);
            0
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

unsafe extern "system" fn prefs_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // pas encore créée, ou message renvoyé par Windows pendant qu'on la modifie : traitement par défaut
    if APP.with(|c| c.try_borrow().map_or(true, |a| a.is_none())) {
        return DefWindowProcW(h, msg, wp, lp);
    }
    // coordonnées logiques relatives au panneau des réglages
    let local = |lp: LPARAM| app(|a| {
        let (x, y) = lparam_xy(lp);
        let p = a.prefs_panel();
        ((x - p.x) / a.scale, (y - p.y) / a.scale)
    });
    match msg {
        WM_LBUTTONDOWN => {
            let (x, y) = local(lp);
            if (78.0..116.0).contains(&y) && (20.0..320.0).contains(&x) {
                app(|a| a.set_theme((((x - 20.0) / 100.0) as usize).min(2)));
            } else if (148.0..182.0).contains(&y) && (8.0..332.0).contains(&x) {
                SetCapture(h);
                app(|a| {
                    a.dragging = true;
                    a.set_opacity_from(x);
                });
            } else if (190.0..234.0).contains(&y) && (262.0..330.0).contains(&x) {
                app(|a| a.toggle_startup());
            }
            0
        }
        WM_MOUSEMOVE => {
            let (x, _) = local(lp);
            app(|a| if a.dragging { a.set_opacity_from(x) });
            0
        }
        WM_LBUTTONUP => {
            if app(|a| std::mem::replace(&mut a.dragging, false)) {
                ReleaseCapture();
                app(|a| shell::save_config(&a.cfg));
            }
            0
        }
        WM_KEYDOWN if wp == VK_ESCAPE as usize => {
            app(|a| a.hide_prefs());
            0
        }
        // clic ailleurs : les réglages se rangent
        WM_ACTIVATE if wp & 0xFFFF == WA_INACTIVE as usize => {
            app(|a| if !a.dragging { a.hide_prefs() });
            0
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

/// Position enregistrée si elle tombe encore sur un écran branché, sinon en haut à gauche.
unsafe fn start_pos(scale: f32) -> (i32, i32) {
    shell::load_pos()
        .filter(|&(x, y)| !MonitorFromPoint(POINT { x: x + 20, y: y + 20 }, MONITOR_DEFAULTTONULL).is_null())
        .unwrap_or(((60.0 * scale) as i32, (120.0 * scale) as i32))
}

fn null_mut<T>() -> *mut T {
    std::ptr::null_mut()
}

unsafe fn class(name: &[u16], proc_: WNDPROC, inst: HINSTANCE) {
    let wc = WNDCLASSW {
        lpfnWndProc: proc_,
        hInstance: inst,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        lpszClassName: name.as_ptr(),
        ..zeroed()
    };
    RegisterClassW(&wc);
}

fn main() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(null(), 2); // COINIT_APARTMENTTHREADED, requis par SHGetFileInfo et ShellLink
        let cfg = shell::load_config();
        shell::set_startup(cfg.startup);
        shell::dark_menus();
        let inst = GetModuleHandleW(null());
        let (main_class, prefs_class) = (shell::w("DESKTOUCH"), shell::w("DESKTOUCH_REGLAGES"));
        class(&main_class, Some(main_proc), inst);
        class(&prefs_class, Some(prefs_proc), inst);
        let h = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            main_class.as_ptr(), main_class.as_ptr(), WS_POPUP, 0, 0, 1, 1, null_mut(), null_mut(), inst, null());
        let ph = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TOPMOST,
            prefs_class.as_ptr(), prefs_class.as_ptr(), WS_POPUP, 0, 0, 1, 1, null_mut(), null_mut(), inst, null());
        MAIN.store(h as isize, Ordering::Relaxed);
        let scale = GetDpiForWindow(h) as f32 / 96.0;
        let (sq_x, sq_y) = start_pos(scale);
        let (items, loaded) = shell::load_items(None).unwrap_or_default();
        let desktop = shell::foreground_is_desktop().unwrap_or(false);
        let mut a = App {
            cfg, scale, main: Surface::new(h), items, loaded, sq_x, sq_y, right: true, down: true,
            open: Spring::new(0.0, 260.0, 1.0),
            vis: Spring::new(if desktop { cfg.opacity } else { 0.0 }, 200.0, 1.0),
            bx: Spring::new(0.0, 520.0, 0.82),
            by: Spring::new(0.0, 520.0, 0.82),
            blob: Spring::new(0.0, 400.0, 1.0),
            press: Spring::new(0.0, 700.0, 0.7),
            hover: -1, tracking: false, desktop,
            prefs: Surface::new(ph), prefs_shown: false,
            p_open: Spring::new(0.0, 320.0, 0.82),
            p_seg: Spring::new(0.0, 420.0, 0.78),
            p_knob: Spring::new(0.0, 900.0, 1.0),
            p_tog: Spring::new(0.0, 520.0, 0.75),
            dragging: false, dirty_main: false, dirty_prefs: false, animating: false, last: Instant::now(),
        };
        a.relayout();
        a.recapture();
        a.draw_main();
        APP.with_borrow_mut(|s| *s = Some(a));
        ShowWindow(h, SW_SHOWNOACTIVATE);
        // juste au-dessus du bureau : visible sur le fond d'écran, caché par toute fenêtre ouverte
        let desk = FindWindowW(shell::w("Progman").as_ptr(), null());
        if !desk.is_null() {
            SetWindowPos(h, desk, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
        // n'apparaître que quand le bureau est au premier plan (événement système, pas de sondage)
        SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, null_mut(), Some(win_event), 0, 0, WINEVENT_OUTOFCONTEXT);
        TASKBAR_CREATED.set(RegisterWindowMessageW(shell::w("TaskbarCreated").as_ptr()));
        shell::tray(h, WM_TRAY, true);

        let mut msg: MSG = zeroed();
        loop {
            if app(|a| a.animating) {
                // animation : une image par rafraîchissement de l'écran, sans jamais bloquer les messages
                while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    if msg.message == WM_QUIT {
                        return;
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                app(|a| a.tick());
                DwmFlush();
            } else if GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            } else {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_test() {
        let (cw, ch) = (88.0, 92.0);
        assert_eq!(hit(1.0, 1.0, 4, 7, cw, ch), 0);
        assert_eq!(hit(cw + 1.0, 1.0, 4, 7, cw, ch), 1);
        assert_eq!(hit(1.0, ch + 1.0, 4, 7, cw, ch), 4);
        assert_eq!(hit(3.0 * cw + 1.0, ch + 1.0, 4, 7, cw, ch), -1); // 8e case vide
        assert_eq!(hit(-1.0, 5.0, 4, 7, cw, ch), -1);
        assert_eq!(hit(4.0 * cw + 1.0, 1.0, 4, 7, cw, ch), -1);
    }

    #[test]
    fn themes_keep_text_readable() {
        // texte foncé sur verre clair, clair sur verre sombre
        assert!(look(0, true).ink[0] < 0.5 && look(0, false).ink[0] > 0.5);
        assert!(look(1, true).ink[0] > 0.5);
        assert!(look(2, false).ink[0] < 0.5);
        assert_eq!(look(1, false).refraction, 0.0);
    }
}
