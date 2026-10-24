# DESKTOUCH

Un petit carré de verre sur le bureau Windows, façon AssistiveTouch : au survol, un panneau de raccourcis en sort comme une goutte. Rust natif, verre liquide calculé maison (réfraction, dispersion, liseré lumineux), animations à ressort cadencées sur l'écran, 0 % de CPU au repos.

## Lancer

```bash
cargo build --release
target/release/desktouch.exe
```

## Utilisation

- Le carré n'apparaît que quand le **bureau** est au premier plan (clic sur le fond d'écran, Win + D).
- Survol : le panneau s'ouvre ; clic sur une tuile : lance l'application. Le carré reste en place.
- Glisser le carré fermé : le déplacer (position gardée). Clic simple : ouvrir / fermer.
- **Ajouter** (dernière tuile, ou menu) : choisis une application, un raccourci ou un jeu dans la fenêtre « Ouvrir ».
- Clic droit sur une tuile : la retirer. Clic droit ailleurs : menu.

## Icône dans la zone de notification

Dans les icônes cachées de la barre des tâches :

- clic : **Réglages** — thème (Liquid Glass, Sombre, Clair), opacité du carré sur le bureau, lancement au démarrage ;
- clic droit : Réglages, Ajouter un raccourci, Ouvrir le dossier des raccourcis, Quitter.

## Fichiers à côté de l'exe

- `Raccourcis/` : les raccourcis du panneau (`.lnk`, `.url`), triés par ordre alphabétique.
- `reglages.txt`, `position.txt` : réglages et position du carré.

## Crédits

Recette du verre adaptée d'[AndroidLiquidGlass](https://github.com/Kyant0/AndroidLiquidGlass) (Kyant, Apache-2.0).
