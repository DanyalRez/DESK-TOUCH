//! Tout ce qui parle à Windows en dehors du rendu : raccourcis, icônes, texte, capture du fond,
//! réglages, icône de notification, sélecteur de fichier.

use crate::render::{Back, Layer, Rgb, Rgba};
use std::{
    ffi::{c_void, OsString},
    mem::{transmute, zeroed},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::{
    core::GUID,
    Win32::{
        Foundation::*,
        Graphics::{Dwm::DwmFlush, Gdi::*},
        System::{
            Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
            LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW},
            Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ},
            Threading::CreateMutexW,
        },
        UI::{
            Controls::{Dialogs::*, ImageList_GetIcon, HIMAGELIST, ILD_TRANSPARENT},
            Shell::*,
            WindowsAndMessaging::*,
        },
    },
};

const IID_IIMAGELIST: GUID = GUID::from_u128(0x46EB5926_582E_4017_9FDF_E8998DAA0950);
const CLSID_SHELLLINK: GUID = GUID::from_u128(0x00021401_0000_0000_C000_000000000046);
const IID_ISHELLLINKW: GUID = GUID::from_u128(0x000214F9_0000_0000_C000_000000000046);
const IID_IPERSISTFILE: GUID = GUID::from_u128(0x0000010B_0000_0000_C000_000000000046);
const SHIL_EXTRALARGE_ID: i32 = 2; // 48 px
const SHIL_JUMBO_ID: i32 = 4; // 256 px
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub struct Item {
    pub name: String,
    pub file: String, // nom du fichier dans le dossier, sert à retenir l'ordre
    pub path: Vec<u16>,                  // terminé par 0, prêt pour ShellExecute
    pub icon: Option<(usize, Vec<u32>)>, // côté, BGRA prémultiplié en pleine définition
}

pub fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

pub fn path_of(wide: &[u16]) -> PathBuf {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    OsString::from_wide(&wide[..end]).into()
}

pub fn beside_exe(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent().map(|p| p.join(name)).unwrap_or_else(|| name.into())
}

pub fn folder() -> PathBuf {
    beside_exe("Shortcuts")
}

/// Une seule instance : sinon deux carrés se superposeraient. Une deuxième instance ouvre les réglages
/// de la première puis s'arrête ; après une mise à jour (`wait`), on attend que l'ancienne ait quitté.
pub unsafe fn single_instance(wait: bool, show_settings: u32) -> bool {
    for _ in 0..30 {
        let m = CreateMutexW(null(), 0, w("DESKTOUCH_instance").as_ptr());
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return true; // le verrou reste ouvert pendant toute la vie de l'app
        }
        CloseHandle(m);
        if !wait {
            let other = FindWindowW(w("DESKTOUCH").as_ptr(), w("DESKTOUCH").as_ptr());
            if !other.is_null() {
                PostMessageW(other, show_settings, 0, WM_LBUTTONUP as isize);
            }
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

/// Exe lancé depuis un dossier de compilation (…\target\…) : il ne s'installe pas et ne touche pas au
/// démarrage de Windows, sinon l'entrée pointerait vers un dossier de travail qui finit par bouger.
pub fn is_dev_build() -> bool {
    let here = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_string_lossy().to_lowercase()));
    here.is_some_and(|h| h.contains("\\target\\") || h.ends_with("\\target"))
}

/// Lancé depuis un téléchargement : s'installe dans %LOCALAPPDATA%\DESKTOUCH, se met dans le menu Démarrer
/// et au démarrage de Windows, lance la copie installée et renvoie vrai (cette instance-ci s'arrête alors).
pub unsafe fn self_install() -> bool {
    let (Ok(exe), Ok(local)) = (std::env::current_exe(), std::env::var("LOCALAPPDATA")) else { return false };
    let dir = PathBuf::from(local).join("DESKTOUCH");
    let here = exe.parent().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default();
    if here == dir.to_string_lossy().to_lowercase() || is_dev_build() {
        return false;
    }
    // une version déjà lancée (installée ou d'ailleurs) se ferme : son exe se libère et elle laisse la place
    let other = FindWindowW(w("DESKTOUCH").as_ptr(), w("DESKTOUCH").as_ptr());
    if !other.is_null() {
        PostMessageW(other, WM_CLOSE, 0, 0);
    }
    let dest = dir.join("DESKTOUCH.exe");
    let _ = std::fs::create_dir_all(&dir);
    let copied = (0..25).any(|i| {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        std::fs::copy(&exe, &dest).is_ok()
    });
    if !copied {
        return false; // pas installable : on tourne simplement là où on est
    }
    // la copie installée s'inscrit aussi à son lancement ; on le fait dès maintenant au cas où elle tarderait
    if load_config().startup {
        write_run_key(&dest);
    }
    if let Ok(roaming) = std::env::var("APPDATA") {
        let menu = PathBuf::from(roaming).join(r"Microsoft\Windows\Start Menu\Programs\DESKTOUCH.lnk");
        let _ = menu.parent().map(std::fs::create_dir_all);
        create_lnk(&dest.to_string_lossy(), &menu);
    }
    ShellExecuteW(null_mut(), w("open").as_ptr(), wide_path(&dest).as_ptr(), null(), null(), SW_SHOWNORMAL);
    true
}

/// Les versions françaises rangeaient tout sous des noms français : on les renomme une fois.
pub fn migrate_files() {
    for (old, new) in [("Raccourcis", "Shortcuts"), ("reglages.txt", "settings.txt"), ("ordre.txt", "order.txt")] {
        let (o, n) = (beside_exe(old), beside_exe(new));
        if o.exists() && !n.exists() {
            let _ = std::fs::rename(o, n);
        }
    }
}

// ---------- réglages ----------

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Config {
    pub theme: usize,     // 0 Liquid Glass, 1 Sombre, 2 Clair
    pub opacity: f32,     // opacité du carré au repos
    pub startup: bool,    // lancement avec Windows
    pub icon_size: usize, // 0 petite, 1 moyenne, 2 grande
    pub cols: usize,      // colonnes du panneau, 3 à 6
    pub names: bool,      // noms sous les logos
    pub click_open: bool, // ouvrir au clic plutôt qu'au survol
}

pub const THEMES: [&str; 3] = ["Liquid Glass", "Dark", "Light"];

impl Default for Config {
    fn default() -> Self {
        Config { theme: 0, opacity: 0.55, startup: true, icon_size: 1, cols: 4, names: true, click_open: false }
    }
}

pub fn parse_config(s: &str) -> Config {
    let mut c = Config::default();
    for line in s.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        // clés anglaises, et les anciennes clés françaises pour les fichiers déjà enregistrés
        match (k.trim(), v.trim()) {
            ("theme", v) => c.theme = v.parse().unwrap_or(0).min(THEMES.len() - 1),
            ("opacity" | "opacite", v) => c.opacity = v.parse::<f32>().map_or(c.opacity, |p| (p / 100.0).clamp(0.15, 1.0)),
            ("startup" | "demarrage", v) => c.startup = v != "0",
            ("icon_size" | "taille", v) => c.icon_size = v.parse().unwrap_or(1).min(2),
            ("columns" | "colonnes", v) => c.cols = v.parse().unwrap_or(4).clamp(3, 6),
            ("names" | "noms", v) => c.names = v != "0",
            ("open" | "ouverture", v) => c.click_open = v == "click" || v == "clic",
            _ => {}
        }
    }
    c
}

pub fn load_config() -> Config {
    std::fs::read_to_string(beside_exe("settings.txt")).map_or_else(|_| Config::default(), |s| parse_config(&s))
}

pub fn save_config(c: &Config) {
    let s = format!("theme={}\nopacity={}\nstartup={}\nicon_size={}\ncolumns={}\nnames={}\nopen={}\n",
        c.theme, (c.opacity * 100.0).round(), c.startup as u8, c.icon_size, c.cols, c.names as u8,
        if c.click_open { "click" } else { "hover" });
    let _ = std::fs::write(beside_exe("settings.txt"), s);
}

/// Position enregistrée « x y ».
pub fn parse_pos(s: &str) -> Option<(i32, i32)> {
    let mut it = s.split_whitespace().map(|n| n.parse().ok());
    Some((it.next()??, it.next()??))
}

pub fn load_pos() -> Option<(i32, i32)> {
    std::fs::read_to_string(beside_exe("position.txt")).ok().and_then(|s| parse_pos(&s))
}

pub fn save_pos(x: i32, y: i32) {
    let _ = std::fs::write(beside_exe("position.txt"), format!("{x} {y}"));
}

pub fn forget_pos() {
    let _ = std::fs::remove_file(beside_exe("position.txt"));
}

/// Rang d'un fichier : sa place dans l'ordre enregistré, sinon après, par ordre alphabétique.
pub fn sort_key(order: &[&str], file: &str) -> (usize, String) {
    let lower = file.to_lowercase();
    (order.iter().position(|o| o.to_lowercase() == lower).unwrap_or(usize::MAX), lower)
}

pub fn save_order(items: &[Item]) {
    let s: Vec<&str> = items.iter().map(|it| it.file.as_str()).collect();
    let _ = std::fs::write(beside_exe("order.txt"), s.join("\n"));
}

/// Lancement à l'ouverture de session ; réécrit à chaque démarrage pour suivre l'exe s'il a bougé.
pub unsafe fn set_startup(on: bool) {
    if is_dev_build() {
        return; // voir is_dev_build : seule la copie installée décide du démarrage
    }
    if !on {
        RegDeleteKeyValueW(HKEY_CURRENT_USER, w(RUN_KEY).as_ptr(), w("DESKTOUCH").as_ptr());
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        write_run_key(&exe);
    }
}

unsafe fn write_run_key(exe: &Path) {
    let cmd = w(&format!("\"{}\"", exe.display()));
    RegSetKeyValueW(HKEY_CURRENT_USER, w(RUN_KEY).as_ptr(), w("DESKTOUCH").as_ptr(), REG_SZ,
        cmd.as_ptr() as _, (cmd.len() * 2) as u32);
}

// ---------- raccourcis ----------

/// Premier lancement : quelques raccourcis de départ pour ne pas ouvrir un panneau vide.
fn write_defaults(dir: &Path) {
    let win = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join("Web.url"), "[InternetShortcut]\r\nURL=https://www.google.com\r\n");
    let _ = std::fs::write(dir.join("Settings.url"), format!(
        "[InternetShortcut]\r\nURL=ms-settings:\r\nIconFile={win}\\ImmersiveControlPanel\\SystemSettings.exe\r\nIconIndex=0\r\n"));
    unsafe {
        create_lnk(&format!(r"{win}\explorer.exe"), &dir.join("Files.lnk"));
        create_lnk(&format!(r"{win}\System32\calc.exe"), &dir.join("Calculator.lnk"));
    }
}

/// Relit le dossier s'il a changé depuis `loaded` (ajout, suppression, renommage).
pub unsafe fn load_items(loaded: Option<std::time::SystemTime>) -> Option<(Vec<Item>, Option<std::time::SystemTime>)> {
    let dir = folder();
    if !dir.exists() {
        write_defaults(&dir);
    }
    let stamp = std::fs::metadata(&dir).and_then(|m| m.modified()).ok();
    if stamp.is_some() && loaded == stamp {
        return None;
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect())
        .unwrap_or_default();
    // ordre choisi par glisser-déposer, puis alphabétique pour les nouveaux venus
    let order = std::fs::read_to_string(beside_exe("order.txt")).unwrap_or_default();
    let order: Vec<&str> = order.lines().collect();
    files.sort_by_key(|p| sort_key(&order, &p.file_name().unwrap_or_default().to_string_lossy()));
    let items = files.iter().map(|p| {
        let path = wide_path(p);
        let name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let file = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Item { name, file, icon: file_icon(&path), path }
    }).collect();
    Some((items, stamp))
}

/// Raccourci .lnk vers `target`, via l'objet COM ShellLink (appelé à la main : windows-sys n'a pas les interfaces).
unsafe fn create_lnk(target: &str, dest: &Path) -> bool {
    type Qi = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32;
    type Release = unsafe extern "system" fn(*mut c_void) -> u32;
    type SetStr = unsafe extern "system" fn(*mut c_void, *const u16) -> i32;
    type Save = unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32;
    let mut link: *mut c_void = null_mut();
    if CoCreateInstance(&CLSID_SHELLLINK, null_mut(), CLSCTX_INPROC_SERVER, &IID_ISHELLLINKW, &mut link) != 0 {
        return false;
    }
    let vt = *(link as *const *const usize);
    let set_path: SetStr = transmute(*vt.add(20)); // IShellLinkW::SetPath
    let set_dir: SetStr = transmute(*vt.add(9)); // IShellLinkW::SetWorkingDirectory
    set_path(link, w(target).as_ptr());
    if let Some(dir) = Path::new(target).parent() {
        set_dir(link, wide_path(dir).as_ptr()); // beaucoup de jeux cherchent leurs fichiers à côté de l'exe
    }
    let qi: Qi = transmute(*vt);
    let release: Release = transmute(*vt.add(2));
    let mut file: *mut c_void = null_mut();
    let mut ok = false;
    if qi(link, &IID_IPERSISTFILE, &mut file) == 0 {
        let fvt = *(file as *const *const usize);
        let save: Save = transmute(*fvt.add(6)); // IPersistFile::Save
        ok = save(file, wide_path(dest).as_ptr(), 1) == 0;
        let frel: Release = transmute(*fvt.add(2));
        frel(file);
    }
    release(link);
    ok
}

/// Fenêtre « Ouvrir » de Windows, dans le menu Démarrer : applications, raccourcis, jeux Steam.
pub unsafe fn pick_file(owner: HWND) -> Option<PathBuf> {
    let mut buf = vec![0u16; 2048];
    let filter: Vec<u16> = "Apps and shortcuts\0*.exe;*.lnk;*.url;*.bat;*.cmd\0All files\0*.*\0\0"
        .encode_utf16().collect();
    let start = std::env::var("ProgramData").map(|p| format!(r"{p}\Microsoft\Windows\Start Menu\Programs")).unwrap_or_default();
    let (start, title) = (w(&start), w("Add a shortcut"));
    let mut ofn: OPENFILENAMEW = zeroed();
    ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrInitialDir = start.as_ptr();
    ofn.lpstrTitle = title.as_ptr();
    // NODEREFERENCELINKS : on veut le raccourci lui-même, avec son nom et son icône
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NODEREFERENCELINKS | OFN_EXPLORER;
    (GetOpenFileNameW(&mut ofn) != 0).then(|| path_of(&buf))
}

/// Ajoute `src` au panneau : copie d'un raccourci, ou nouveau .lnk vers un programme.
pub unsafe fn add_shortcut(src: &Path) -> bool {
    let dir = folder();
    let _ = std::fs::create_dir_all(&dir);
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Shortcut".into());
    let ext = src.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let free = |e: &str| (1..).map(|i| if i == 1 { dir.join(format!("{stem}.{e}")) } else { dir.join(format!("{stem} {i}.{e}")) })
        .find(|p| !p.exists()).expect("nom libre");
    match ext.as_str() {
        "lnk" | "url" => std::fs::copy(src, free(&ext)).is_ok(),
        _ => create_lnk(&src.to_string_lossy(), &free("lnk")),
    }
}

pub fn remove_shortcut(path: &[u16]) -> bool {
    let p = path_of(path);
    p.starts_with(folder()) && std::fs::remove_file(p).is_ok()
}

// ---------- images ----------

/// DIB 32 bits haut-en-bas : (bitmap, pointeur vers les pixels).
pub unsafe fn dib(w: i32, h: i32) -> (HBITMAP, *mut u32) {
    let mut bmi: BITMAPINFO = zeroed();
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h,
        biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, ..zeroed()
    };
    let mut bits: *mut c_void = null_mut();
    let bmp = CreateDIBSection(null_mut(), &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    (bmp, bits as *mut u32)
}

/// Logo du fichier tel que l'Explorateur l'affiche, en 256 px quand il existe.
unsafe fn file_icon(path: &[u16]) -> Option<(usize, Vec<u32>)> {
    let mut sfi: SHFILEINFOW = zeroed();
    if SHGetFileInfoW(path.as_ptr(), 0, &mut sfi, size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX) == 0 {
        return None;
    }
    for (list_id, size) in [(SHIL_JUMBO_ID, 256usize), (SHIL_EXTRALARGE_ID, 48)] {
        let mut list: *mut c_void = null_mut();
        if SHGetImageList(list_id, &IID_IIMAGELIST, &mut list) != 0 {
            continue;
        }
        let hicon = ImageList_GetIcon(list as HIMAGELIST, sfi.iIcon, ILD_TRANSPARENT);
        if hicon.is_null() {
            continue;
        }
        let (bmp, bits) = dib(size as i32, size as i32);
        let dc = CreateCompatibleDC(null_mut());
        let old = SelectObject(dc, bmp);
        // sur un fond à 0, DrawIconEx produit directement du BGRA prémultiplié
        DrawIconEx(dc, 0, 0, hicon, size as i32, size as i32, 0, null_mut(), DI_NORMAL);
        GdiFlush();
        let mut px = std::slice::from_raw_parts(bits, size * size).to_vec();
        SelectObject(dc, old);
        DeleteDC(dc);
        DeleteObject(bmp);
        DestroyIcon(hicon);
        if px.iter().all(|p| p >> 24 == 0) {
            // vieille icône sans canal alpha : tout pixel non noir devient opaque
            px.iter_mut().filter(|p| **p != 0).for_each(|p| *p |= 0xFF00_0000);
        }
        // beaucoup d'applis n'ont pas de 256 px : Windows pose alors une petite icône dans un coin
        if size == 256 && opaque_extent(&px, size) <= 64 {
            continue;
        }
        return Some((size, px));
    }
    None
}

fn opaque_extent(px: &[u32], size: usize) -> usize {
    let (mut maxx, mut maxy) = (0, 0);
    for (i, p) in px.iter().enumerate() {
        if p >> 24 > 16 {
            maxx = maxx.max(i % size);
            maxy = maxy.max(i / size);
        }
    }
    maxx.max(maxy) + 1
}

fn unpack(p: u32) -> Rgba {
    [(p >> 16 & 255) as f32 / 255.0, (p >> 8 & 255) as f32 / 255.0, (p & 255) as f32 / 255.0, (p >> 24) as f32 / 255.0]
}

/// Réduction de qualité : moyenne de k×k échantillons par pixel de destination (pas de crénelage).
pub fn downsample(src: &[u32], ss: usize, d: usize) -> Vec<Rgba> {
    let k = (ss / d.max(1)).clamp(1, 8);
    let mut out = vec![[0.0; 4]; d * d];
    for y in 0..d {
        for x in 0..d {
            let mut acc = [0.0f32; 4];
            for j in 0..k {
                for i in 0..k {
                    let sx = ((x * k + i) as f32 + 0.5) * ss as f32 / (d * k) as f32;
                    let sy = ((y * k + j) as f32 + 0.5) * ss as f32 / (d * k) as f32;
                    let p = unpack(src[(sy as usize).min(ss - 1) * ss + (sx as usize).min(ss - 1)]);
                    (0..4).for_each(|c| acc[c] += p[c]);
                }
            }
            out[y * d + x] = acc.map(|a| a / (k * k) as f32);
        }
    }
    out
}

/// Texte à poser sur un calque.
pub struct Text<'a> {
    pub s: &'a str,
    pub r: RECT,
    pub size: f32,
    pub weight: i32,
    pub flags: DRAW_TEXT_FORMAT,
    pub level: f32, // 1 = plein, < 1 = texte secondaire
    pub glyph: bool, // police d'icônes Segoe Fluent
}

/// Calque de textes : GDI dessine en blanc sur noir, la luminosité devient le canal alpha (bords nets).
pub unsafe fn text_layer(pw: usize, ph: usize, texts: &[Text], ink: Rgb) -> Layer {
    let (bmp, bits) = dib(pw as i32, ph as i32);
    let dc = CreateCompatibleDC(null_mut());
    let old = SelectObject(dc, bmp);
    SetBkMode(dc, TRANSPARENT as i32);
    for t in texts {
        let face = if t.glyph { "Segoe Fluent Icons" } else { "Segoe UI Variable Text" };
        let font = CreateFontW(-t.size.round() as i32, 0, 0, 0, t.weight, 0, 0, 0, DEFAULT_CHARSET as u32, 0, 0,
            ANTIALIASED_QUALITY as u32, 0, w(face).as_ptr());
        let of = SelectObject(dc, font);
        let g = (255.0 * t.level).round() as u32;
        SetTextColor(dc, g | g << 8 | g << 16);
        let txt: Vec<u16> = t.s.encode_utf16().collect();
        let mut r = t.r;
        DrawTextW(dc, txt.as_ptr(), txt.len() as i32, &mut r, t.flags | DT_NOPREFIX | if t.flags & DT_WORDBREAK == 0 { DT_SINGLELINE } else { 0 });
        SelectObject(dc, of);
        DeleteObject(font);
    }
    GdiFlush();
    let px = std::slice::from_raw_parts(bits, pw * ph).iter().map(|&p| {
        let a = ((p >> 16 & 255).max(p >> 8 & 255).max(p & 255)) as f32 / 255.0;
        [ink[0] * a, ink[1] * a, ink[2] * a, a]
    }).collect();
    SelectObject(dc, old);
    DeleteDC(dc);
    DeleteObject(bmp);
    Layer { w: pw, h: ph, px }
}

pub struct Cell {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Calque d'une tuile (logo + nom) ; `None` : la tuile « Ajouter ». Une tuile = un calque, pour
/// pouvoir les faire glisser une par une. Le nom tient sur deux lignes plutôt que d'être coupé.
#[allow(clippy::too_many_arguments)]
pub unsafe fn tile_layer(item: Option<&Item>, cw: usize, ch: usize, icon: usize, icon_top: i32, names: bool, s: f32, ink: Rgb) -> Layer {
    let ix = (cw - icon) as i32 / 2;
    let mut texts = Vec::new();
    if names {
        texts.push(Text {
            s: item.map_or("Add", |it| it.name.as_str()),
            r: RECT { left: (4.0 * s) as i32, right: cw as i32 - (4.0 * s) as i32,
                top: icon_top + icon as i32 + (4.0 * s) as i32, bottom: ch as i32 - (1.0 * s) as i32 },
            size: 11.5 * s, weight: 500, flags: DT_CENTER | DT_TOP | DT_WORDBREAK | DT_EDITCONTROL | DT_END_ELLIPSIS,
            level: 1.0, glyph: false,
        });
    }
    if item.is_none() {
        texts.push(Text { s: "\u{E710}", r: RECT { left: ix, right: ix + icon as i32, top: icon_top, bottom: icon_top + icon as i32 },
            size: icon as f32 * 0.55, weight: 400, flags: DT_CENTER | DT_VCENTER, level: 1.0, glyph: true });
    }
    let mut layer = text_layer(cw, ch, &texts, ink);
    if let Some((ss, raw)) = item.and_then(|it| it.icon.as_ref()) {
        let ic = downsample(raw, *ss, icon);
        for y in 0..icon {
            for x in 0..icon {
                let (dx, dy) = (ix as usize + x, icon_top as usize + y);
                if dx < cw && dy < ch {
                    let (src, d) = (ic[y * icon + x], &mut layer.px[dy * cw + dx]);
                    *d = std::array::from_fn(|k| src[k] + d[k] * (1.0 - src[3]));
                }
            }
        }
    }
    layer
}

/// Copie de l'écran sous la fenêtre, sans la fenêtre elle-même (exclue de la capture le temps de la copie).
pub unsafe fn capture(hwnd: HWND, x: i32, y: i32, cw: usize, ch: usize) -> Back {
    SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    DwmFlush();
    let screen = GetDC(null_mut());
    let (bmp, bits) = dib(cw as i32, ch as i32);
    let dc = CreateCompatibleDC(screen);
    let old = SelectObject(dc, bmp);
    BitBlt(dc, 0, 0, cw as i32, ch as i32, screen, x, y, SRCCOPY);
    GdiFlush();
    let px = std::slice::from_raw_parts(bits, cw * ch).iter().map(|&p| {
        let c = unpack(p);
        [c[0], c[1], c[2]]
    }).collect();
    SelectObject(dc, old);
    DeleteDC(dc);
    DeleteObject(bmp);
    ReleaseDC(null_mut(), screen);
    SetWindowDisplayAffinity(hwnd, WDA_NONE);
    Back { w: cw, h: ch, px }
}

// ---------- zone de notification ----------

/// Icône de la zone de notification : le logo intégré à l'exe (assets/desktouch.ico), à la taille du système.
unsafe fn tray_icon() -> HICON {
    let n = GetSystemMetrics(SM_CXSMICON);
    let icon = LoadImageW(GetModuleHandleW(null()), 1 as _, IMAGE_ICON, n, n, LR_DEFAULTCOLOR);
    if icon.is_null() { LoadIconW(null_mut(), IDI_APPLICATION) } else { icon as HICON }
}

pub unsafe fn tray(hwnd: HWND, msg: u32, add: bool) {
    let mut nid: NOTIFYICONDATAW = zeroed();
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    if !add {
        Shell_NotifyIconW(NIM_DELETE, &nid);
        return;
    }
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = msg;
    nid.hIcon = tray_icon();
    for (d, c) in nid.szTip.iter_mut().zip("DESKTOUCH".encode_utf16()) {
        *d = c;
    }
    Shell_NotifyIconW(NIM_ADD, &nid);
}

/// Menus en mode sombre quand Windows l'est (fonctions non documentées d'uxtheme, sans effet sinon).
pub unsafe fn dark_menus() {
    let ux = LoadLibraryW(w("uxtheme.dll").as_ptr());
    if ux.is_null() {
        return;
    }
    if let Some(f) = GetProcAddress(ux, 135 as _) {
        let set_mode: unsafe extern "system" fn(i32) -> i32 = transmute(f); // SetPreferredAppMode
        set_mode(1); // AllowDark : suit le thème de Windows
    }
    if let Some(f) = GetProcAddress(ux, 136 as _) {
        let flush: unsafe extern "system" fn() = transmute(f); // FlushMenuThemes
        flush();
    }
}

/// Menu contextuel natif ; renvoie l'identifiant choisi (0 si rien).
pub unsafe fn menu(hwnd: HWND, entries: &[(usize, &str)]) -> usize {
    let m = CreatePopupMenu();
    for &(id, label) in entries {
        if id == 0 {
            AppendMenuW(m, MF_SEPARATOR, 0, null());
        } else {
            AppendMenuW(m, MF_STRING, id, w(label).as_ptr());
        }
    }
    let mut p: POINT = zeroed();
    GetCursorPos(&mut p);
    SetForegroundWindow(hwnd); // sans ça, le menu ne se ferme pas en cliquant ailleurs
    let id = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, p.x, p.y, 0, hwnd, null());
    DestroyMenu(m);
    id as usize
}

/// Accroche la fenêtre au bureau, comme un gadget : elle a le bureau pour propriétaire, reste donc juste
/// au-dessus du fond d'écran, sous toutes les autres fenêtres, et Win + D ne la cache pas.
pub unsafe fn pin_to_desktop(hwnd: HWND) {
    let desk = FindWindowW(w("Progman").as_ptr(), null());
    if desk.is_null() {
        return;
    }
    SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, desk as isize);
    // tout en bas : une fenêtre possédée ne passe jamais sous son propriétaire, elle s'arrête donc sur le bureau
    SetWindowPos(hwnd, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pos_parsing() {
        assert_eq!(parse_pos("60 120"), Some((60, 120)));
        assert_eq!(parse_pos("-1500 40\n"), Some((-1500, 40))); // écran à gauche du principal
        assert_eq!(parse_pos("60"), None);
        assert_eq!(parse_pos("abc 3"), None);
    }

    #[test]
    fn saved_order_first_then_alphabetical() {
        let order = ["Steam.lnk", "web.url"];
        let mut files = vec!["Calculatrice.lnk", "Web.url", "Steam.lnk", "Bloc-notes.lnk"];
        files.sort_by_key(|f| sort_key(&order, f));
        assert_eq!(files, ["Steam.lnk", "Web.url", "Bloc-notes.lnk", "Calculatrice.lnk"]);
    }

    #[test]
    fn config_parsing() {
        assert_eq!(parse_config(""), Config::default());
        let c = parse_config("theme=2\nopacite=30\ndemarrage=0\n");
        assert_eq!((c.theme, c.startup), (2, false));
        assert!((c.opacity - 0.3).abs() < 1e-6);
        // valeurs hors bornes ou illisibles : ramenées à quelque chose de sûr
        let c = parse_config("theme=9\nopacite=2\nautre=1");
        assert_eq!(c.theme, 2);
        assert!((c.opacity - 0.15).abs() < 1e-6);
        assert_eq!(parse_config("opacite=abc").opacity, Config::default().opacity);
        let c = parse_config("taille=2
colonnes=9
noms=0
ouverture=clic");
        assert_eq!((c.icon_size, c.cols, c.names, c.click_open), (2, 6, false, true));
        // clés anglaises (format actuel)
        let e = parse_config("theme=1\nopacity=40\nstartup=0\nicon_size=0\ncolumns=5\nnames=0\nopen=click\n");
        assert_eq!(e, Config { theme: 1, opacity: 0.4, startup: false, icon_size: 0, cols: 5, names: false, click_open: true });
        // aller-retour par le fichier (anciennes clés françaises)
        let c = Config { theme: 1, opacity: 0.4, startup: false, icon_size: 0, cols: 5, names: false, click_open: true };
        let s = format!("theme=1
opacite=40
demarrage=0
taille=0
colonnes=5
noms=0
ouverture=clic
");
        assert_eq!(parse_config(&s), c);
    }

    #[test]
    fn downsample_averages() {
        // damier blanc / transparent 2×2 → gris à moitié opaque
        let src = vec![0xFFFF_FFFF, 0, 0, 0xFFFF_FFFF];
        let d = downsample(&src, 2, 1);
        assert!((d[0][3] - 0.5).abs() < 1e-4 && (d[0][0] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn small_icon_in_jumbo_canvas_is_detected() {
        let mut px = vec![0u32; 256 * 256];
        for y in 0..32 {
            for x in 0..32 {
                px[y * 256 + x] = 0xFF00_0000;
            }
        }
        assert_eq!(opaque_extent(&px, 256), 32);
    }

    #[test]
    fn wide_path_round_trip() {
        let p = Path::new(r"C:\Jeux\Élan.lnk");
        assert_eq!(path_of(&wide_path(p)), p);
    }
}

#[cfg(test)]
mod com_tests {
    use super::*;

    #[test]
    #[ignore] // écrit un .lnk dans le dossier Raccourcis à côté de l'exe de test
    fn add_exe_creates_working_lnk() {
        unsafe { windows_sys::Win32::System::Com::CoInitializeEx(null(), 2) };
        let win = std::env::var("WINDIR").unwrap();
        let notepad = PathBuf::from(format!(r"{win}\System32\notepad.exe"));
        assert!(unsafe { add_shortcut(&notepad) });
        let lnk = folder().join("notepad.lnk");
        assert!(lnk.exists());
        assert!(unsafe { file_icon(&wide_path(&lnk)) }.is_some());
        assert!(remove_shortcut(&wide_path(&lnk)));
        assert!(!remove_shortcut(&wide_path(&notepad)), "jamais en dehors du dossier");
    }
}
