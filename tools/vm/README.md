# Banc d'essai en machine virtuelle

Une VM Ubuntu 26.04 jetable (QEMU + KVM, sans sudo sur l'hôte) où Okapi tourne comme chez un utilisateur : session GNOME, pkexec et polkit, PackageKit, snapd, flatpak, Docker, Orca, deux comptes humains. Claude la pilote seul : commandes par SSH, interface par le bus d'accessibilité (AT-SPI, qu'AccessKit remplit pour Slint), écrans par la QMP de QEMU.

## Mise en route (une fois)

```bash
tools/vm/okapi-vm fetch
```

```bash
tools/vm/okapi-vm prepare
```

`fetch` télécharge l'image cloud (862 Mo) et vérifie sa somme. `prepare` en fait l'image de base par cloud-init (quelques minutes, environ 1,5 Go de paquets tirés par la VM) : GNOME minimal en connexion automatique de `tester`, accessibilité active, ni veille, ni verrouillage, ni bannière de notification, timers d'apt coupés. Les comptes et leurs mots de passe de test sont dans `test-accounts.env` : `tester` (administrateur, celui de la session) et `autre` (sans droits). Tout ce qui est lourd vit dans `~/.cache/okapi-vm/` (`OKAPI_VM_HOME` pour ailleurs), jamais dans le dépôt.

## Lancer des scénarios

```bash
tools/vm/okapi-vm run cleaner_admin
```

Chaque scénario démarre sur une surcouche neuve de l'image de base, avec l'Okapi de cette copie (compilé sur l'hôte en debug, installé dans la VM aux chemins de `packaging/linux/install-helper.sh`), puis la surcouche est jetée : rien ne passe d'un essai à l'autre. Sans nom, `run` les passe tous ; `--keep` laisse la dernière VM allumée.

Ce qui reste sur l'hôte, dans `~/.cache/okapi-vm/runs/<date>-<scénario>/` :

- `steps.log` : chaque étape, horodatée ;
- `NN-<étape>.png` : les captures, dans l'ordre ;
- `okapi-state/` : le `~/.local/state/okapi/` de `tester` (journaux de l'interface et du helper) ;
- `journal.txt` : le journal systemd du démarrage (polkit, pkexec, démons) ;
- à l'échec : `NN-echec.png`, `tree.txt` (l'arbre d'accessibilité à ce moment), `error.txt` pour une exception imprévue ;
- `result.json`.

## Explorer à la main

```bash
tools/vm/okapi-vm start
```

Puis `okapi-vm ui -- tree` (l'arbre de la fenêtre d'Okapi), `okapi-vm ui -- click --name Analyser --role button`, `okapi-vm ui -- wait --pattern '^Nettoyage' --timeout 120`, `okapi-vm ui -- applications`, `okapi-vm shot /chemin/ecran.png`, `okapi-vm ssh -- <commande>` (en `tester`, `--root` pour root), et `okapi-vm stop` pour tout jeter. Le pilote (`guest/atspi_driver.py`) attend dans la VM : un `click` attend que le nœud s'affiche, refuse un nœud désactivé et demande `--index` quand plusieurs correspondent.

## Ajouter un scénario

Un fichier `scenarios/<nom>.py` (le nom est celui de `run`), dont la première ligne de docstring dit ce qu'il essaie, et une fonction `run(bench)`. `bench` offre :

- `bench.ui.click(nom, role=..., pattern=..., index=...)`, `bench.ui.wait(nom, role=..., state=..., gone=..., timeout=...)`, `bench.ui.find(...)`, `bench.ui.tree()` : les noms sont les textes accessibles affichés, en français (la VM est en `fr_FR`), les rôles ceux d'AT-SPI (`button`, `check-box`, `label`, `frame`...) ;
- `bench.screenshot("etape")` ;
- `bench.user("commande")` (en `tester`), `bench.root("commande")` ;
- `bench.allow_elevation_without_password()` : une règle polkit propre à la VM laisse lancer le helper sans mot de passe (sans elle, pkexec attend la fenêtre de mot de passe de GNOME) ;
- `bench.advance_clock(secondes)` : avance l'horloge de la VM (NTP coupé), ce qui vieillit d'un coup tout ce qui existe ;- `bench.launch_app()`, `bench.stop_app()` ;
- `bench.check(condition, "ce qui doit être vrai")`, `bench.log("...")`.

Un échec est une `ScenarioFailure` (ce que lèvent `check` et les attentes du pilote) : le scénario s'arrête, la capture et l'arbre du moment sont gardés. Pour trouver les noms, `okapi-vm start` puis `okapi-vm ui -- tree`. `scenarios/cleaner_admin.py` sert de modèle : il pose des fichiers, avance l'horloge de deux heures pour les vieillir, analyse, obtient les droits, nettoie, lit le reçu et vérifie que les fichiers sont partis.

## Pièges

- L'âge minimal se mesure sur la date de changement d'inode, que `touch -d` ne recule pas : un fichier posé par un scénario est neuf. Et le helper refuse tout âge minimal sous une heure (`HELPER_MIN_AGE_FLOOR`), quels que soient les réglages : pour les cibles administrateur, `advance_clock` est le seul moyen.
- Le pilote relit un nœud dès qu'il le trouve : une page remplacée pendant la lecture fait relire l'arbre, pas échouer.
- Okapi est lancé par `systemd-run --user`, comme GNOME lance une appli : il n'appartient pas à la session SSH, et pkexec juge la session graphique.
- Une seule VM à la fois (`~/.cache/okapi-vm/machine/`). `okapi-vm stop` la jette si un essai a été interrompu.
