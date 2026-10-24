//! Intègre l'icône (logo DESKTOUCH) dans l'exe : visible dans l'Explorateur et réutilisée par la zone de notification.

fn main() {
    println!("cargo:rerun-if-changed=assets/desktouch.rc");
    println!("cargo:rerun-if-changed=assets/desktouch.ico");
    let obj = format!("{}/desktouch-res.o", std::env::var("OUT_DIR").unwrap());
    // shortcut: windres vient de la chaîne MinGW (cible GNU) ; avec MSVC il faudrait rc.exe
    let ok = std::process::Command::new("windres")
        .args(["assets/desktouch.rc", "-O", "coff", "-o", &obj])
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        println!("cargo:rustc-link-arg-bins={obj}");
    } else {
        println!("cargo:warning=windres introuvable : l'exe n'aura pas d'icône");
    }
}
