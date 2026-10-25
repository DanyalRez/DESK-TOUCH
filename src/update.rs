//! Mises à jour : la dernière release GitHub du projet, téléchargée puis mise à la place de l'exe.
//! Tout se fait dans un thread à part ; la fenêtre n'attend jamais le réseau.

use crate::shell::{w, wide_path};
use std::path::{Path, PathBuf};
use windows_sys::Win32::{
    System::Com::{CoInitializeEx, Urlmon::URLDownloadToFileW},
    UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
};

const API: &str = "https://api.github.com/repos/DanyalRez/DESK-TOUCH/releases/latest";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub tag: String,
    pub url: String, // l'exe à télécharger
}

/// "v1.2" → (1, 2, 0) ; None si ce n'est pas une version.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut it = s.trim().trim_start_matches(['v', 'V']).split('.').map(|p| p.parse::<u32>().ok());
    let major = it.next()??;
    Some((major, it.next().flatten().unwrap_or(0), it.next().flatten().unwrap_or(0)))
}

pub fn is_newer(tag: &str) -> bool {
    matches!((parse_version(tag), parse_version(VERSION)), (Some(a), Some(b)) if a > b)
}

/// Valeur texte de `key` à partir de `from` dans un JSON (sans dépendance : on ne lit que deux champs).
fn string_field(json: &str, key: &str) -> Option<String> {
    let at = json.find(key)? + key.len();
    let rest = json[at..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            c => out.push(c),
        }
    }
    None
}

/// Réponse de l'API GitHub : le nom de la release et le premier fichier .exe joint.
pub fn parse_release(json: &str) -> Option<Release> {
    let tag = string_field(json, "\"tag_name\"")?;
    let url = json
        .match_indices("\"browser_download_url\"")
        .filter_map(|(i, _)| string_field(&json[i..], "\"browser_download_url\""))
        .find(|u| u.to_lowercase().ends_with(".exe"))?;
    Some(Release { tag, url })
}

unsafe fn download(url: &str, dest: &Path) -> bool {
    CoInitializeEx(std::ptr::null(), 0); // COINIT_MULTITHREADED, propre à ce thread
    let _ = std::fs::remove_file(dest);
    URLDownloadToFileW(std::ptr::null_mut(), w(url).as_ptr(), wide_path(dest).as_ptr(), 0, std::ptr::null_mut()) == 0
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(name)
}

/// Ok(Some) : une version plus récente existe ; Ok(None) : déjà à jour ; Err : réseau ou release illisible.
pub fn check() -> Result<Option<Release>, ()> {
    let file = temp("desktouch-release.json");
    // paramètre bidon : contourne le cache Internet de Windows, qui resservirait une vieille réponse
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if !unsafe { download(&format!("{API}?t={stamp}"), &file) } {
        return Err(());
    }
    let json = std::fs::read_to_string(&file).map_err(|_| ())?;
    let rel = parse_release(&json).ok_or(())?;
    Ok(is_newer(&rel.tag).then_some(rel))
}

/// Télécharge la nouvelle version, la met à la place de l'exe et la lance. L'appelant quitte ensuite.
pub fn install(rel: &Release) -> bool {
    let new = temp("desktouch-update.exe");
    if !unsafe { download(&rel.url, &new) } {
        return false;
    }
    // un vrai exe Windows commence par « MZ » : on ne remplace jamais l'app par une page d'erreur
    let ok = std::fs::read(&new).is_ok_and(|b| b.len() > 100_000 && b.starts_with(b"MZ"));
    let Ok(exe) = std::env::current_exe() else { return false };
    if !ok {
        return false;
    }
    // un exe en cours d'exécution ne peut pas être écrasé, mais il peut être renommé
    let old = exe.with_extension("old");
    let _ = std::fs::remove_file(&old);
    if std::fs::rename(&exe, &old).is_err() {
        return false;
    }
    if std::fs::copy(&new, &exe).is_err() {
        let _ = std::fs::rename(&old, &exe);
        return false;
    }
    unsafe {
        ShellExecuteW(std::ptr::null_mut(), w("open").as_ptr(), wide_path(&exe).as_ptr(), w("--updated").as_ptr(),
            std::ptr::null(), SW_SHOWNORMAL);
    }
    true
}

/// Après une mise à jour : l'ancien exe renommé ne sert plus.
pub fn cleanup_old() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("old"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("2.0"), Some((2, 0, 0)));
        assert_eq!(parse_version("latest"), None);
        assert!(parse_version("v99.0.0") > parse_version(VERSION));
        assert!(is_newer("v99.0.0"));
        assert!(!is_newer(&format!("v{VERSION}")));
        assert!(!is_newer("v0.0.1"));
        assert!(!is_newer("nightly"));
    }

    #[test]
    fn release_json() {
        let json = r#"{"url":"x","tag_name": "v1.4.0","assets":[
            {"name":"notes.txt","browser_download_url":"https://github.com/a/b/releases/download/v1.4.0/notes.txt"},
            {"name":"DESKTOUCH.exe","browser_download_url":"https://github.com/a/b/releases/download/v1.4.0/DESKTOUCH.exe"}]}"#;
        assert_eq!(parse_release(json), Some(Release {
            tag: "v1.4.0".into(),
            url: "https://github.com/a/b/releases/download/v1.4.0/DESKTOUCH.exe".into(),
        }));
        assert_eq!(parse_release(r#"{"message":"Not Found"}"#), None);
        assert_eq!(parse_release(r#"{"tag_name":"v2","assets":[]}"#), None); // pas d'exe joint
    }
}
