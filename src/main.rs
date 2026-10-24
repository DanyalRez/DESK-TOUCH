#![windows_subsystem = "windows"]
//! DESKTOUCH : un petit carré de verre posé sur le bureau ; au survol, un panneau de raccourcis en
//! sort comme une goutte. Il vit sur le bureau, sous toutes les fenêtres, comme une icône.
//! Zéro CPU au repos : on ne calcule des images que pendant une animation, cadencées sur l'écran.

mod render;
mod shell;

use render::{smoothstep, Back, Frame, Layer, Look, Pill, Rgb, Rr, Spring};
use shell::{Cell, Config, Item, Text};
use std::{
    cell::{Cell as StdCell, RefCell},
    mem::zeroed,
    ptr::null,
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::DwmFlush, Gdi::*},
    System::{Com::CoInitializeEx, LibraryLoader::GetModuleHandleW},
    UI::{
        HiDpi::{GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
        Input::KeyboardAndMouse::*,
        Shell::ShellExecuteW,
        WindowsAndMessaging::*,
    },
};

const WM_MOUSELEAVE: u32 = 0x02A3; // absent de windows-sys
const WM_DPICHANGED: u32 = 0x02E0;
const WM_TRAY: u32 = WM_APP + 1;
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
const ICON_SIZES: [f32; 3] = [36.0, 46.0, 58.0];
const MARGIN: f32 = 30.0; // place pour l'ombre et le rebond du ressort
const PREFS_W: f32 = 340.0;
const PREFS_TOP: f32 = 54.0; // sous le titre « Réglages »

/// Contrôle d'une ligne des réglages.
enum Ctl {
    Seg(&'static [&'static str]),
    Slider,
    Toggle,
}

const ROWS: [(&str, Ctl); 7] = [
    ("Thème", Ctl::Seg(&shell::THEMES)),
    ("Taille des icônes", Ctl::Seg(&["Petite", "Moyenne", "Grande"])),
    ("Colonnes", Ctl::Seg(&["3", "4", "5", "6"])),
    ("Ouverture du panneau", Ctl::Seg(&["Au survol", "Au clic"])),
    ("Opacité du logo", Ctl::Slider),
    ("Afficher les noms", Ctl::Toggle),
    ("Lancer au démarrage", Ctl::Toggle),
];

fn row_height(c: &Ctl) -> f32 {
    match c {
        Ctl::Seg(_) => 66.0,
        Ctl::Slider => 58.0,
        Ctl::Toggle => 42.0,
    }
}

/// Haut de chaque ligne (pixels logiques, relatif au panneau) et hauteur totale.
fn row_tops() -> ([f32; ROWS.len()], f32) {
    let mut tops = [0.0; ROWS.len()];
    let mut y = PREFS_TOP;
    for (t, (_, c)) in tops.iter_mut().zip(&ROWS) {
        *t = y;
        y += row_height(c);
    }
    (tops, y + 10.0)
}

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
    // tuiles : un calque chacune, position animée (relative au panneau) pour le glisser-déposer
    tiles: Vec<Layer>,
    tx: Vec<Spring>,
    ty: Vec<Spring>,
    press_sq: Option<(f32, f32)>,             // bouton enfoncé sur le carré (clic ou début de déplacement)
    press_tile: Option<(usize, (f32, f32))>, // bouton enfoncé sur une tuile (clic ou début de glisser)
    drag: Option<usize>,                     // tuile en train d'être déplacée
    grab: (f32, f32),                        // où on l'a attrapée, dans la tuile
    cursor: (f32, f32),
    // réglages
    prefs: Surface,
    prefs_shown: bool,
    p_open: Spring,
    p_rows: Vec<Spring>, // une par ligne : case choisie, curseur ou interrupteur
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
        self.tiles().clamp(1, self.cfg.cols)
    }
    /// Taille du logo, en pixels logiques.
    fn icon(&self) -> f32 {
        ICON_SIZES[self.cfg.icon_size]
    }
    /// Case d'une tuile : plus haute avec les noms (deux lignes), carrée sans.
    fn cell(&self) -> (f32, f32) {
        let i = self.icon();
        if self.cfg.names { (i + 40.0, i + 52.0) } else { (i + 22.0, i + 22.0) }
    }
    fn panel_size(&self) -> (f32, f32) {
        let rows = self.tiles().div_ceil(self.cfg.cols);
        let (cw, ch) = self.cell();
        (self.px(PAD * 2.0 + cw * self.cols() as f32).round(), self.px(PAD * 2.0 + ch * rows as f32).round())
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
        let (cw, ch) = self.cell();
        (0..self.tiles()).map(|i| Cell {
            x: self.px(PAD + (i % cols) as f32 * cw).round() as i32,
            y: self.px(PAD + (i / cols) as f32 * ch).round() as i32,
            w: self.px(cw).round() as i32,
            h: self.px(ch).round() as i32,
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

    /// Recrée le calque de chaque tuile et les pose à leur place.
    unsafe fn rebuild_main_layer(&mut self) {
        let cells = self.cells();
        let ink = self.look(&self.main).ink;
        let (cw, ch, icon) = (cells[0].w as usize, cells[0].h as usize, self.px(self.icon()).round() as usize);
        // sans les noms, le logo est centré dans sa case
        let top = if self.cfg.names { self.px(8.0) as i32 } else { (ch - icon) as i32 / 2 };
        let (names, s) = (self.cfg.names, self.scale);
        self.tiles = (0..cells.len()).map(|i| shell::tile_layer(self.items.get(i), cw, ch, icon, top, names, s, ink)).collect();
        self.tx = cells.iter().map(|c| Spring::new(c.x as f32, 380.0, 0.8)).collect();
        self.ty = cells.iter().map(|c| Spring::new(c.y as f32, 380.0, 0.8)).collect();
        self.dirty_main = true;
    }

    /// Glisser-déposer : la tuile `from` prend la place `to`, les autres se décalent en douceur.
    fn move_tile(&mut self, from: usize, to: usize) {
        let it = self.items.remove(from);
        self.items.insert(to, it);
        let l = self.tiles.remove(from);
        self.tiles.insert(to, l);
        let (sx, sy) = (self.tx.remove(from), self.ty.remove(from));
        self.tx.insert(to, sx);
        self.ty.insert(to, sy);
        for (i, c) in self.cells().iter().enumerate() {
            self.tx[i].target = c.x as f32;
            self.ty[i].target = c.y as f32;
        }
        self.drag = Some(to);
        self.kick();
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
        if self.tracking || self.open.target > 0.5 {
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
            rr: Rr { x: self.bx.x + inset, y: self.by.x + inset, w: self.px(self.cell().0) - 2.0 * inset,
                h: self.px(self.cell().1) - 2.0 * inset, r: self.px(self.cell().0.min(self.cell().1) * 0.24) },
            color: accent(&look), amount: 0.15 * blob, rim: 0.55 * blob,
        }];
        let slide = (1.0 - content) * self.px(8.0);
        let mut layers: Vec<(&Layer, (i32, i32), f32)> = (0..self.tiles.len()).filter(|&i| Some(i) != self.drag).map(|i| {
            (&self.tiles[i], ((panel.x + self.tx[i].x).round() as i32, (panel.y + self.ty[i].x + slide).round() as i32), content)
        }).collect();
        if let Some(d) = self.drag {
            // la tuile tenue suit la souris, par-dessus les autres
            let pos = ((self.cursor.0 - self.grab.0).round() as i32, (self.cursor.1 - self.grab.1).round() as i32);
            layers.push((&self.tiles[d], pos, content));
        }
        let f = Frame {
            back: &self.main.back,
            layers: &layers,
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

    fn set_hover(&mut self, mx: f32, my: f32) {
        let p = self.panel();
        let hv = if self.open.x > 0.85 && self.open.target == 1.0 {
            hit(mx - p.x - self.px(PAD), my - p.y - self.px(PAD), self.cols(), self.tiles(), self.px(self.cell().0), self.px(self.cell().1))
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
        Rr { x: m, y: m, w: self.px(PREFS_W), h: self.px(row_tops().1), r: self.px(PANEL_R) }
    }

    /// Valeur affichée par la ligne `i` : position du curseur, case choisie ou interrupteur (0 / 1).
    fn row_value(&self, i: usize) -> f32 {
        let c = &self.cfg;
        match i {
            0 => c.theme as f32,
            1 => c.icon_size as f32,
            2 => (c.cols - 3) as f32,
            3 => c.click_open as u8 as f32,
            4 => (c.opacity - 0.15) / 0.85,
            5 => c.names as u8 as f32,
            _ => c.startup as u8 as f32,
        }
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
        for i in 0..ROWS.len() {
            let v = self.row_value(i);
            self.p_rows[i].snap(v);
        }
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
        let label = |t: &'static str, rect: RECT, level: f32, flags| Text { s: t, r: rect, size: 12.5 * s, weight: 500, flags, level, glyph: false };
        let pct = format!("{} %", (self.cfg.opacity * 100.0).round());
        let (tops, _) = row_tops();
        let mut texts = vec![Text { s: "Réglages", r: r(22.0, 16.0, 296.0, 26.0), size: 17.0 * s, weight: 600,
            flags: DT_LEFT | DT_VCENTER, level: 1.0, glyph: false }];
        for (i, (name, ctl)) in ROWS.iter().enumerate() {
            let y = tops[i];
            match ctl {
                Ctl::Seg(opts) => {
                    texts.push(label(name, r(22.0, y, 296.0, 18.0), 0.62, DT_LEFT | DT_VCENTER));
                    let w = 300.0 / opts.len() as f32;
                    for (k, o) in opts.iter().enumerate() {
                        let mut t = label(o, r(20.0 + k as f32 * w, y + 22.0, w, 34.0), 1.0, DT_CENTER | DT_VCENTER);
                        t.weight = 600;
                        texts.push(t);
                    }
                }
                Ctl::Slider => texts.push(label(name, r(22.0, y, 200.0, 18.0), 0.62, DT_LEFT | DT_VCENTER)),
                Ctl::Toggle => {
                    let mut t = label(name, r(22.0, y, 230.0, 40.0), 1.0, DT_LEFT | DT_VCENTER);
                    t.size = 13.5 * s;
                    texts.push(t);
                }
            }
        }
        let mut value = label("", r(220.0, tops[4], 100.0, 18.0), 0.9, DT_RIGHT | DT_VCENTER);
        value.s = &pct;
        value.weight = 600;
        texts.push(value);
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
        let pill = |rr: Rr, color: Rgb, amount: f32, rim: f32| Pill { rr, color, amount: amount * content, rim: rim * content };
        let (tops, _) = row_tops();
        let mut pills = Vec::new();
        for (i, (_, ctl)) in ROWS.iter().enumerate() {
            let (y, v) = (tops[i], self.p_rows[i].x);
            match ctl {
                Ctl::Seg(opts) => {
                    let w = 300.0 / opts.len() as f32;
                    pills.push(pill(at(20.0, y + 22.0, 300.0, 34.0, 17.0), ac, 0.08, 0.0));
                    pills.push(pill(at(23.0 + v * w, y + 25.0, w - 6.0, 28.0, 14.0), ac, 0.2, 0.7));
                }
                Ctl::Slider => {
                    let knob = 20.0 + v.clamp(0.0, 1.0) * (300.0 - 22.0);
                    pills.push(pill(at(20.0, y + 30.0, 300.0, 6.0, 3.0), ac, 0.16, 0.0));
                    pills.push(pill(at(20.0, y + 30.0, knob - 20.0 + 11.0, 6.0, 3.0), ac, 0.6, 0.0));
                    pills.push(pill(at(knob, y + 22.0, 22.0, 22.0, 11.0), [1.0; 3], 1.0, 0.6));
                }
                Ctl::Toggle => {
                    pills.push(pill(at(272.0, y + 6.0, 48.0, 28.0, 14.0), ac, lerp(0.14, 0.6, v), 0.3));
                    pills.push(pill(at(275.0 + v * 20.0, y + 9.0, 22.0, 22.0, 11.0), [1.0; 3], 1.0, 0.6));
                }
            }
        }
        let f = Frame {
            back: &self.prefs.back,
            layers: &[(&self.prefs.layer, (p.x as i32, p.y as i32), content)],
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

    /// Clic sur la ligne `i` à l'abscisse logique `x` (relative au panneau des réglages).
    unsafe fn prefs_click(&mut self, i: usize, x: f32) {
        match ROWS[i].1 {
            Ctl::Seg(opts) => {
                let k = (((x - 20.0) / (300.0 / opts.len() as f32)).max(0.0) as usize).min(opts.len() - 1);
                match i {
                    0 => self.cfg.theme = k,
                    1 => self.cfg.icon_size = k,
                    2 => self.cfg.cols = k + 3,
                    _ => self.cfg.click_open = k == 1,
                }
            }
            Ctl::Slider => {
                self.dragging = true;
                return self.set_opacity_from(x);
            }
            Ctl::Toggle if i == 5 => self.cfg.names = !self.cfg.names,
            Ctl::Toggle => {
                self.cfg.startup = !self.cfg.startup;
                shell::set_startup(self.cfg.startup);
            }
        }
        self.p_rows[i].target = self.row_value(i);
        shell::save_config(&self.cfg);
        match i {
            // thème : nouvelles couleurs de texte partout
            0 => {
                self.rebuild_main_layer();
                self.rebuild_prefs_layer();
            }
            // taille, colonnes, noms : le panneau change de forme
            1 | 2 | 5 => {
                self.relayout();
                self.recapture();
                self.draw_main();
            }
            _ => {}
        }
        self.kick();
    }

    unsafe fn set_opacity_from(&mut self, lx: f32) {
        let t = ((lx - 31.0) / 278.0).clamp(0.0, 1.0);
        self.cfg.opacity = 0.15 + t * 0.85;
        self.p_rows[4].target = t;
        self.vis.target = self.vis_target();
        self.rebuild_prefs_layer();
        self.kick();
    }

    // ---------- boucle d'animation ----------

    fn main_springs(&mut self) -> Vec<&mut Spring> {
        let mut v = vec![&mut self.open, &mut self.vis, &mut self.bx, &mut self.by, &mut self.blob, &mut self.press];
        v.extend(self.tx.iter_mut().chain(self.ty.iter_mut()));
        v
    }
    fn prefs_springs(&mut self) -> Vec<&mut Spring> {
        std::iter::once(&mut self.p_open).chain(self.p_rows.iter_mut()).collect()
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

fn moved(a: (f32, f32), b: (f32, f32), min: f32) -> bool {
    (a.0 - b.0).abs() > min || (a.1 - b.1).abs() > min
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
            // bouton enfoncé sur le carré et la souris bouge : on le déplace (Windows suit la souris)
            let grab_square = app(|a| {
                let start = a.press_sq?;
                if !moved(start, (x, y), a.px(4.0)) {
                    return None;
                }
                a.press_sq = None;
                a.tracking = false; // Windows prend la souris pendant le déplacement : on réarme le survol après
                a.close();
                a.open.snap(0.0);
                a.blob.snap(0.0);
                a.draw_main();
                Some(())
            });
            if grab_square.is_some() {
                KillTimer(h, T_OPEN);
                ReleaseCapture();
                SendMessageW(h, WM_NCLBUTTONDOWN, HTCAPTION as usize, 0);
                return 0;
            }
            // bouton enfoncé sur une tuile et la souris bouge : glisser-déposer pour changer l'ordre
            let dragging = app(|a| {
                if let Some((i, start)) = a.press_tile {
                    if a.drag.is_none() && moved(start, (x, y), a.px(6.0)) {
                        let p = a.panel();
                        a.grab = (start.0 - p.x - a.tx[i].x, start.1 - p.y - a.ty[i].x);
                        a.drag = Some(i);
                        a.blob.target = 0.0;
                        a.press.target = 0.0;
                    }
                }
                let Some(d) = a.drag else { return false };
                a.cursor = (x, y);
                let p = a.panel();
                let j = hit(x - p.x - a.px(PAD), y - p.y - a.px(PAD), a.cols(), a.items.len(), a.px(a.cell().0), a.px(a.cell().1));
                if j >= 0 && j as usize != d {
                    a.move_tile(d, j as usize);
                }
                a.dirty_main = true;
                a.kick();
                true
            });
            if dragging {
                return 0;
            }
            let enter = app(|a| !std::mem::replace(&mut a.tracking, true));
            if enter {
                let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                TrackMouseEvent(&mut tme);
                KillTimer(h, T_CLOSE);
                let closed = app(|a| {
                    a.vis.target = a.vis_target();
                    a.kick();
                    a.open.target < 0.5 && !a.cfg.click_open // réglage « au clic » : le survol n'ouvre pas
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
            let inside = app(|a| {
                a.press_sq.is_some() || a.press_tile.is_some()
                    || a.over_shape((c.x - a.main.win.left) as f32, (c.y - a.main.win.top) as f32)
            });
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
        // on attend de voir si la souris bouge : clic (ouvrir, lancer) ou glisser (déplacer, réordonner)
        WM_LBUTTONDOWN => {
            let (x, y) = lparam_xy(lp);
            let capture = app(|a| {
                if a.square().contains(x, y) {
                    a.press_sq = Some((x, y));
                    return true;
                }
                if a.open.x > 0.85 && a.hover >= 0 {
                    a.press.target = 1.0;
                    a.kick();
                    if (a.hover as usize) < a.items.len() {
                        a.press_tile = Some((a.hover as usize, (x, y)));
                    }
                    return true;
                }
                false
            });
            if capture {
                KillTimer(h, T_OPEN);
                SetCapture(h);
            }
            0
        }
        WM_LBUTTONUP => {
            enum Click { Toggle, Launch(Vec<u16>), Add, Dropped, Nothing }
            let click = app(|a| {
                a.press.target = 0.0;
                a.kick();
                if a.press_sq.take().is_some() {
                    return Click::Toggle;
                }
                let tile = a.press_tile.take();
                if let Some(d) = a.drag.take() {
                    // la tuile lâchée glisse depuis la souris jusqu'à sa place
                    let p = a.panel();
                    a.tx[d].x = a.cursor.0 - a.grab.0 - p.x;
                    a.ty[d].x = a.cursor.1 - a.grab.1 - p.y;
                    shell::save_order(&a.items);
                    a.hover = -1;
                    return Click::Dropped;
                }
                if a.open.x <= 0.85 || a.hover < 0 {
                    return Click::Nothing;
                }
                match a.items.get(a.hover as usize) {
                    Some(it) if tile.is_some() => {
                        let p = it.path.clone();
                        a.close();
                        Click::Launch(p)
                    }
                    None => Click::Add,
                    _ => Click::Nothing,
                }
            });
            ReleaseCapture();
            match click {
                Click::Toggle => app(|a| if a.open.target > 0.5 { a.close() } else { a.begin_open() }),
                Click::Launch(path) => {
                    ShellExecuteW(null_mut(), shell::w("open").as_ptr(), path.as_ptr(), null(), null(), SW_SHOWNORMAL);
                }
                Click::Add => add_flow(h),
                Click::Dropped | Click::Nothing => {}
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
                a.relayout();
                a.recapture();
                a.draw_main();
            });
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
            // ligne touchée : la zone du contrôle (segments, curseur) ou toute la ligne (interrupteurs)
            let (tops, _) = row_tops();
            let row = ROWS.iter().enumerate().position(|(i, (_, c))| {
                let t = tops[i];
                match c {
                    Ctl::Seg(_) => (t + 22.0..t + 56.0).contains(&y) && (20.0..320.0).contains(&x),
                    Ctl::Slider => (t + 14.0..t + 50.0).contains(&y) && (8.0..332.0).contains(&x),
                    Ctl::Toggle => (t..t + 40.0).contains(&y),
                }
            });
            if let Some(i) = row {
                if matches!(ROWS[i].1, Ctl::Slider) {
                    SetCapture(h);
                }
                app(|a| a.prefs_click(i, x));
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

/// Place de départ, en haut à gauche de l'écran principal : le carré y revient à chaque lancement
/// (le déplacer ne vaut que pour la session, demande de l'utilisateur).
fn start_pos(scale: f32) -> (i32, i32) {
    ((60.0 * scale) as i32, (120.0 * scale) as i32)
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
        let scale = GetDpiForWindow(h) as f32 / 96.0;
        let (sq_x, sq_y) = start_pos(scale);
        let (items, loaded) = shell::load_items(None).unwrap_or_default();
        let mut a = App {
            cfg, scale, main: Surface::new(h), items, loaded, sq_x, sq_y, right: true, down: true,
            open: Spring::new(0.0, 260.0, 1.0),
            vis: Spring::new(cfg.opacity, 200.0, 1.0),
            bx: Spring::new(0.0, 520.0, 0.82),
            by: Spring::new(0.0, 520.0, 0.82),
            blob: Spring::new(0.0, 400.0, 1.0),
            press: Spring::new(0.0, 700.0, 0.7),
            hover: -1, tracking: false,
            tiles: Vec::new(), tx: Vec::new(), ty: Vec::new(),
            press_sq: None, press_tile: None, drag: None, grab: (0.0, 0.0), cursor: (0.0, 0.0),
            prefs: Surface::new(ph), prefs_shown: false,
            p_open: Spring::new(0.0, 320.0, 0.82),
            p_rows: ROWS.iter().map(|(_, c)| match c {
                Ctl::Seg(_) => Spring::new(0.0, 420.0, 0.78),
                Ctl::Slider => Spring::new(0.0, 900.0, 1.0),
                Ctl::Toggle => Spring::new(0.0, 520.0, 0.75),
            }).collect(),
            dragging: false, dirty_main: false, dirty_prefs: false, animating: false, last: Instant::now(),
        };
        a.relayout();
        a.recapture();
        a.draw_main();
        APP.with_borrow_mut(|s| *s = Some(a));
        ShowWindow(h, SW_SHOWNOACTIVATE);
        shell::pin_to_desktop(h); // après l'affichage, qui sinon la remonte devant les fenêtres
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
