# DESKTOUCH

Un petit carré discret sur le bureau Windows qui s'ouvre en panneau de raccourcis au survol, façon AssistiveTouch. Rust natif, fond acrylique, ~0 % de CPU au repos.

## Lancer

```bash
cargo build --release
target/release/desktouch.exe
```

## Raccourcis

Le panneau affiche les fichiers du dossier `Raccourcis` placé à côté de `desktouch.exe` (créé au premier lancement avec quelques exemples).

- Glisse-y n'importe quel raccourci : `.lnk` du menu Démarrer, `.url` de jeux Steam, un dossier en raccourci…
- Le logo vient de Windows, le nom du fichier sert de libellé.
- Tri par ordre alphabétique : préfixe `1 `, `2 `… pour choisir l'ordre.
- La tuile « Ajouter » ouvre ce dossier. Les changements sont pris en compte à la prochaine ouverture du panneau.

## Utilisation

- Survol : le panneau s'ouvre ; clic sur une tuile : lance le raccourci.
- Glisser le carré fermé : le déplacer.
- Clic droit : quitter.
