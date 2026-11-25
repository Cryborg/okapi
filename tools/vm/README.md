# Banc d'essai en machine virtuelle

Des VM Ubuntu 26.04 jetables (QEMU + KVM, sans sudo sur l'hôte) où Okapi tourne comme chez un utilisateur : session GNOME, pkexec et polkit, PackageKit, snapd, flatpak, Docker, Orca, deux comptes humains. Claude les pilote seul : commandes par SSH, interface par le bus d'accessibilité (AT-SPI, qu'AccessKit remplit pour Slint), écrans par la QMP de QEMU. Jusqu'à trois tournent en même temps, une par emplacement.

## Mise en route (une fois)

```bash
tools/vm/okapi-vm fetch
```

```bash
tools/vm/okapi-vm prepare
```

`fetch` télécharge l'image cloud (862 Mo) et vérifie sa somme. `prepare` en fait l'image de base par cloud-init (quelques minutes, environ 2 Go tirés par la VM) : GNOME minimal en connexion automatique de `tester`, accessibilité active, ni veille, ni verrouillage, ni bannière de notification, timers d'apt coupés. Elle porte aussi ce dont les scénarios ont besoin sans réseau : wl-clipboard, auditd, bindfs, dosfstools, dpkg-dev et apt-utils, gnupg, le serveur NFS, libpam-tmpdir (installé mais coupé), et côté Flathub l'appli `io.mpv.Mpv` avec son runtime plus un second `org.freedesktop.Platform` que rien n'utilise. `prepare --force` la refait (refusé tant qu'un emplacement sert) ; ce qu'on y ajoute va dans `guest/provision.sh`. Les comptes et leurs mots de passe de test sont dans `test-accounts.env` : `tester` (administrateur, celui de la session) et `autre` (sans droits). Tout ce qui est lourd vit dans `~/.cache/okapi-vm/` (`OKAPI_VM_HOME` pour ailleurs), jamais dans le dépôt.

Pour la couverture, une fois aussi (sans sudo : `llvm-tools` de rustup et `rustfilt`) :

```bash
tools/vm/install-coverage-tools.sh
```

## Lancer des scénarios

```bash
tools/vm/okapi-vm run cleaner_admin
```

```bash
tools/vm/okapi-vm run 'cleaner_*' --tag admin
```

`run` prend des noms ou des motifs du shell (entre apostrophes, sinon bash les développe), et `--tag` (répétable) ne garde que les scénarios qui portent tous ces tags ; sans nom ni tag, il les passe tous. Un motif qui ne nomme aucun scénario est une erreur. `okapi-vm list` affiche chaque scénario avec ses tags et sa docstring. `--repeat N` passe la sélection N fois de suite (chaque passage sur une VM neuve), pour traquer un échec au hasard des délais.

Chaque scénario démarre sur une surcouche neuve de l'image de base, avec l'Okapi de cette copie (compilé sur l'hôte en debug une fois pour la série, copié à part pour qu'un autre build n'y touche pas, installé dans la VM aux chemins de `packaging/linux/install-helper.sh`), puis la surcouche est jetée : rien ne passe d'un essai à l'autre. `--keep` laisse la dernière VM allumée.

Ce qui reste sur l'hôte, dans `~/.cache/okapi-vm/runs/<date>-<emplacement>-<scénario>/` :

- `steps.log` : chaque étape, horodatée ;
- `NN-<étape>.png` : les captures, dans l'ordre ;
- `okapi-state/` : le `~/.local/state/okapi/` de `tester` (journaux de l'interface et du helper, historique) ;
- `journal.txt` : le journal systemd du démarrage (polkit, pkexec, démons) ;
- à l'échec : `NN-echec.png`, `tree.txt` (l'arbre d'accessibilité à ce moment), `error.txt` pour une exception imprévue ;
- `result.json`.

## Emplacements

Un emplacement (1 à 3) porte une VM : son dossier `~/.cache/okapi-vm/slots/<N>/` (surcouche, QMP, journal série, binaires installés) et son port SSH fixe, `52200 + N`. L'image de base et la clé sont communes. `run` et `start` prennent le premier emplacement libre, ou celui de `--slot N` (à défaut, `OKAPI_VM_SLOT`), et refusent net quand il n'y en a plus : trois VM à 4 Go, c'est ce que l'hôte (30 Go) tient à côté des builds cargo. Un emplacement est occupé tant qu'un `run` le tient (verrou sur `slots/<N>/lock`, relâché quand le processus finit, même tué) ou que sa VM tourne (laissée par `start` ou `--keep`).

`ssh`, `ui`, `shot` et `stop` visent la seule VM allumée, ou celle de `--slot N` quand il y en a plusieurs. `stop` refuse de jeter la VM d'un `run` en cours.

## Couverture

```bash
tools/vm/okapi-vm run --coverage cleaner_admin
```

```bash
tools/vm/okapi-vm coverage report
```

`--coverage` compile une version instrumentée (`-C instrument-coverage`, dans `target/coverage/` pour ne pas invalider le build normal), garde ses binaires dans `~/.cache/okapi-vm/coverage/builds/<empreinte>/`, et à la fin de chaque scénario rapatrie tous les profils `.profraw` de la VM à côté. `coverage report` les fusionne tous et écrit, dans `~/.cache/okapi-vm/coverage/report/` : `files.txt` (par fichier), `functions.txt` (par fonction) et `html/index.html`. Le rapport se fait contre le build instrumenté le plus récent : une fonction inchangée depuis un build plus ancien garde ses comptes, une fonction modifiée ne compte que les runs du nouveau code. `coverage reset` repart de zéro.

Ce qui est mesuré : l'interface, le CLI lancé par `common.cli` et le helper lancé par pkexec. Un `okapi` lancé à la main par `bench.user(...)` écrit aussi son profil (dans le dossier courant, retrouvé quand même), mais au milieu des fichiers de l'utilisateur : passer par `common.cli`.

## Explorer à la main

```bash
tools/vm/okapi-vm start
```

Puis `okapi-vm ui -- tree` (l'arbre de la fenêtre d'Okapi), `okapi-vm ui -- click --name Analyser --role button`, `okapi-vm ui -- wait --pattern '^Nettoyage' --timeout 120`, `okapi-vm ui -- applications`, `okapi-vm shot /chemin/ecran.png`, `okapi-vm ssh -- <commande>` (en `tester`, `--root` pour root), et `okapi-vm stop` pour tout jeter. Le pilote (`guest/atspi_driver.py`) attend dans la VM : un `click` attend que le nœud s'affiche, refuse un nœud désactivé et demande `--index` quand plusieurs correspondent.

## Écrire un scénario

Un fichier `scenarios/<nom>.py` (le nom est celui de `run`), dont la première ligne de docstring dit ce qu'il essaie, une ligne `TAGS = [...]` (le domaine, `admin` s'il passe par le helper...) et une fonction `run(bench)`. `scenarios/cleaner_admin.py` sert de modèle. Chaque scénario part d'une VM neuve : il pose lui-même tout ce dont il a besoin.

`bench` offre :

- `bench.ui.click(nom, role=..., pattern=..., index=...)`, `bench.ui.wait(nom, role=..., state=..., gone=..., timeout=...)`, `bench.ui.find(...)`, `bench.ui.tree()` : les noms sont les textes accessibles affichés, en français (la VM est en `fr_FR`), les rôles ceux d'AT-SPI (`button`, `check-box`, `label`, `frame`, `text`...) ;
- `bench.ui.node(...)` : un nœud, avec ses états, sa description, sa valeur, son texte et sa place dans sa fenêtre (`extents`, `[x, y, largeur, hauteur]`) ; `bench.ui.center(nœud)` : le pixel de l'écran au milieu de ce nœud ; `bench.ui.set_text(..., text="...")` remplace le contenu d'un champ au clavier et attend que le champ le lise ;
- `bench.ui.reveal(nom, role=..., within="Éléments trouvés")` : une liste virtualisée ne montre à AT-SPI que ses lignes visibles ; remonte la liste nommée `within` puis la fait défiler à la molette jusqu'à ce que le nœud paraisse ; `bench.scroll_at(x, y, crans)` : la molette à un pixel de l'écran (vers le haut si négatif) ;
- `bench.shell` : la même chose dans l'arbre de GNOME Shell, pour ses propres fenêtres (mot de passe de polkit...) ;
- `bench.key("Escape", "Ctrl+a", "Alt+F4")` : des touches au clavier de la VM (noms dans `KEY_QCODES` de `bench.py`, ou une lettre) ; `bench.type_text("Texte 2.0")` tape du texte là où est le focus (lettres et chiffres ASCII, et la ponctuation de `AZERTY_CHARACTERS`) ; `bench.click_at(x, y)` : un clic de souris à un pixel de l'écran ;
- `bench.screenshot("etape")` ;
- `bench.user("commande")` (en `tester`), `bench.other("commande")` (en `autre`), `bench.root("commande")`, `bench.run_as(compte, "commande")` : la sortie, et un code non nul fait échouer ; `bench.execute(compte, "commande")` rend un `Outcome` (`code`, `stdout`, `stderr` à part) quel que soit le code ;
- `bench.set_elevation(Elevation.WITHOUT_PASSWORD | REFUSED | PASSWORD)` : une règle polkit propre à la VM dit ce que pkexec répond au helper (`PASSWORD` : la fenêtre de GNOME, à remplir ou annuler par `bench.shell`) ; `bench.allow_elevation_without_password()` en est le raccourci ; `bench.elevation_by_policy()` retire cette règle : la politique d'Okapi répond seule (`auth_admin_keep`, mot de passe demandé une fois puis gardé quelques minutes) ;
- `bench.advance_clock(secondes)` : avance l'horloge de la VM (NTP coupé), ce qui vieillit d'un coup tout ce qui existe ;
- `bench.launch_app(environnement, window=...)` : l'interface avec des variables en plus (`LANG`, `TMPDIR`...), sans attendre sa fenêtre si `window=False` ; `bench.app_exit_code()` attend qu'elle se termine seule et rend son code ; `bench.stop_app()` ;
- `bench.restart_session()` : redémarre gdm, qui ferme la session de `tester` et le reconnecte ; ce qui ne vaut qu'à l'ouverture de session (un groupe, un module PAM) s'applique ;
- `bench.check(condition, "ce qui doit être vrai")`, `bench.log("...")`.

Et `okapivm.common`, pour ce qui revient partout :

- `Files(compte, chemins, size=...)` : des fichiers d'un compte (`bench.accounts.admin.name`, `bench.accounts.other.name` ou `common.ROOT`), chemins absolus ou en `~/` ;
- `plant(bench, *fichiers, age_seconds=...)` : les écrit (dossiers créés, contenu aléatoire, au compte qui les possède), puis avance l'horloge pour les vieillir ;
- `present(bench, fichiers)`, `check_gone(bench, *fichiers)`, `check_kept(bench, *fichiers)` ;
- `open_screen(bench, "Doublons")` : va sur une page de la barre latérale, en ouvrant son groupe s'il est fermé, et attend qu'elle soit la page affichée ;
- `read_store(bench, "settings.json", compte)`, `write_store(bench, nom, valeur, compte)`, `store_path(nom)` : les fichiers du magasin d'Okapi (`STORE_FILES` dit où vit chacun ; `history.jsonl` en liste de lignes), `None` quand il n'existe pas ; `last_receipt(bench, compte=...)` : la dernière entrée de l'historique ; `last_deletions(bench, compte=...)` : le journal des suppressions du dernier nettoyage (`path`, `result`, `reason` de chaque entrée), écrit quand `deletionLog` est allumé ; `write_settings(bench, {"minAgeMinutes": 120, ...})` : les réglages remplacés, au format que lit cet Okapi ;
- `expand_home(bench, "~/x", compte)` : le chemin absolu (une exclusion ou un dossier à nettoyer des réglages, le chemin d'une remarque) ; `journal(bench, "okapi-app.log", compte)` (et `journal_text`, le texte brut) : les événements JSON du journal d'un binaire, lignes DEBUG comprises ; celui de l'interface reçoit aussi les traces du helper ;
- `cli(bench, "--clean", account=..., environment={...})` (un `Outcome` ; `environment` ajoute des variables, `HOME`, `TMPDIR`...) et `cli_json(bench, ..., code=0)` (le document de `--json`) : le CLI dans la VM, mesuré quand la couverture l'est ;
- `helper(bench, requête, account=ROOT)` (un `Outcome`) : une ligne de requête (texte ou dict) écrite au helper installé, son entrée tenue ouverte jusqu'à sa fin (la fin de l'entrée l'annule) ; `helper_answer(outcome)` : sa réponse JSON ;
- `enable_pam_tmpdir(bench)` : allume pam_tmpdir et refait la session, `TMPDIR` devient `/tmp/user/<uid>` ;
- `mount_fat(bench, point)`, `bind_mount(bench, source, point, fuse=...)` (noyau, ou bindfs par FUSE), `unmount(bench, point)` ;
- `Package(nom, version, application=...)`, `build_deb(bench, paquet)`, `install_deb(bench, paquet)`, `publish_apt_repository(bench, *paquets)` : des paquets Debian faits dans la VM et un dépôt APT local signé, qu'APT et PackageKit voient ;
- `open_fake_browser(bench, firefox_profile=..., chromium_profile=...)`, `close_fake_browser(bench)` : un processus qui tient un profil comme un navigateur ouvert (`.parentlock` verrouillé, le nom sous Linux, visible dans `/proc/locks` ; `SingletonLock` qui le nomme) ; `stubborn=True`, il ignore SIGTERM (et `close_fake_browser` le tue par SIGKILL).

Et `okapivm.cleaner`, pour la page Nettoyeur (ses textes et les noms des groupes, `CRASH_REPORTS`, `JOURNAL_LOGS`... y sont des constantes) :

- `open_page`, `choose(bench, "Jeux")` (les autres cartes décochées), `is_checked` et `set_checked`, `fold(bench, "Système", folded=...)` (la liste des résultats ne montre que les lignes à l'écran), `show(bench, "Navigateurs", "Chrome")` (cette catégorie seule dépliée, avec ces navigateurs) ;
- `analyze` (rend le titre de fin) ; `scan(bench)` : l'interface lancée, la page ouverte, une analyse qui doit finir sur « Résultats » ;
- `obtain_rights(bench)` (jusqu'à « Droits obtenus »), `row(bench, groupe)` (la case d'un groupe, la liste défilée jusqu'à elle), `rows(bench)` (toutes les cases, la liste parcourue de haut en bas), `check_groups(bench, cleaner.CRASH_REPORTS, ...)` (ces groupes cochés, ceux de root sont proposés décochés), `explain(bench, cleaner.WHAT_GOES, groupe)` (le bouton d'explication d'un groupe, `SAFE` ou `WHAT_GOES`, déplie ce que disent ses règles) ;
- `confirm` (la confirmation acceptée, sans attendre la fin) et `clean(bench, recorded=...)` (jusqu'à « Nettoyage terminé », puis le reçu enregistré dans l'historique, sauf `recorded=False`) ;
- `last_scan(bench)` : les champs de la trace « scan finished » (`entries`, `bytes`, `issues`), écrite aussi pour une analyse partielle, quand `last-analysis.json` ne l'est que pour une analyse d'au moins les catégories par défaut.

`scenarios/_admin.py` garde ce qui n'est qu'aux scénarios des droits : le bandeau des droits (`banner`, `wait_banner`, `waiting`) et l'âge qui dépasse le plancher du helper (`AGING_SECONDS`).

`scenarios/bench_system.py` et `scenarios/bench_interface.py` (tag `bench`) essaient chacune de ces primitives : à relancer quand on touche au banc.

Un échec est une `ScenarioFailure` (ce que lèvent `check` et les attentes du pilote) : le scénario s'arrête, la capture et l'arbre du moment sont gardés. Pour trouver les noms, `okapi-vm start` puis `okapi-vm ui -- tree`.

## Pièges

- L'âge minimal se mesure sur la date de changement d'inode, que `touch -d` ne recule pas : un fichier posé par un scénario est neuf. Et le helper refuse tout âge minimal sous une heure (`HELPER_MIN_AGE_FLOOR`), quels que soient les réglages : pour les cibles administrateur, avancer l'horloge est le seul moyen.
- L'interface ignore exprès un clic qui suit un changement d'écran de moins de `SECOND_CLICK` (le second clic d'un double clic, `crates/app/src/cleaner.rs`) : un pilote qui clique dès qu'un nœud apparaît perd ce clic, au hasard des délais (vu sur une case cochée 383 ms après « Droits obtenus »). `bench.ui.click` laisse donc le nœud en place `SECOND_CLICK` plus 200 ms avant de cliquer (valeur lue dans le source, jamais recopiée), et une case qui ne change pas d'état après son action fait échouer le clic. `okapi-vm ui -- click` à la main ne patiente pas, sauf `--settle`.
- Le pilote relit un nœud dès qu'il le trouve : une page remplacée pendant la lecture fait relire l'arbre, pas échouer.
- Okapi est lancé par `systemd-run --user`, comme GNOME lance une appli : il n'appartient pas à la session SSH, et pkexec juge la session graphique.
- L'interface s'arrête sur SIGTERM, qui saute l'écriture du profil à la sortie : la couverture tourne en mode continu (`%c` dans `LLVM_PROFILE_FILE`), qui sous Linux exige `-C llvm-args=-runtime-counter-relocation`. Sans lui, le profil d'un processus tué est vide.
- pkexec vide l'environnement : le helper ne voit pas `LLVM_PROFILE_FILE` et écrit `default_<signature>_<pid>.profraw` dans son dossier courant. Le banc cherche les profils dans toute la VM plutôt que de l'élever autrement.
- Les noms de fonctions Rust sont manglés (v0) : sans `rustfilt`, `functions.txt` est illisible.
- Le rapport HTML lit les sources à leur chemin de compilation : un worktree supprimé depuis ne s'affiche plus.
- La VM a un clavier AZERTY : `bench.key` et `type_text` envoient des codes de touche physiques (qcodes), traduits (`a` est la touche `q` du QWERTY, les chiffres demandent Maj). QEMU répond avant que les touches soient tapées : relire ce qu'elles ont fait en l'attendant, comme `set_text`.
- AccessKit dit `enabled sensitive` de tout bouton, même désactivé (seuls les rôles qui ont une lecture seule, comme les cases, la perdent) : un bouton désactivé se prouve en cliquant dessus et en constatant que rien ne se passe.
- Une case à trois états (une catégorie des résultats) ne dit « partielle » que dans sa description : `bench.ui.click` attend donc que l'état coché ou la description change.
- La corbeille demande ses 30 jours deux fois : par la date du `.trashinfo`, et par la date de changement de l'entrée elle-même. Avancer l'horloge de plus de 30 jours.
- AccessKit n'offre pas l'interface EditableText à AT-SPI : un champ ne se remplit qu'au clavier, après un clic dedans.
- Wayland ne dit à personne où est une fenêtre : AT-SPI donne la place d'un nœud dans sa fenêtre, même demandée à l'écran. `ui.center` tient la fenêtre d'Okapi pour maximisée (ce que GNOME fait d'elle sur l'écran de 1280×800 de la VM), son contenu finissant au coin bas droit de l'écran ; GNOME Shell, lui, est l'écran.
- Une modale d'Okapi est un cadre sans nom dans le cadre « Okapi » : `ui.center` cherche la fenêtre par son nom. Une boucle du pilote prend environ une seconde, plus que `SECOND_CLICK` : un geste plus rapide (le double clic de CLN-024) se joue en clics de souris.
- Les boutons de GNOME Shell (la fenêtre de mot de passe) n'ont pas d'action AT-SPI : `bench.click_at(*bench.shell.center(...))`. Les rôles sont ceux d'AT-SPI récent : `button`, plus `push-button`.
- `click_at` passe par une tablette virtuelle (`virtio-tablet-pci`) en coordonnées absolues : une souris relative tomberait n'importe où.
- gdm ne connecte `tester` automatiquement qu'à son démarrage : après une simple déconnexion (`loginctl terminate-user`), il attend sur son écran de connexion. D'où le redémarrage de gdm.
- Les commandes du banc passent par une connexion SSH maîtresse ouverte au démarrage : son environnement ne voit pas un module PAM allumé depuis. Ce que l'interface reçoit se lit dans `systemctl --user show-environment`.
- Le `SingletonLock` de Chromium est un lien vers rien (`<hôte>-<pid>`) : `[ -e ]` le suit et ne trouve rien, `[ -L ]` le voit.
- `RUSTFLAGS` instrumente aussi les scripts de build et les macros procédurales, qui écriraient leurs profils dans la copie de travail : le build instrumenté les envoie dans `target/coverage/build-profiles/`.
- Un `SLINT_BACKEND` inconnu ne fait pas échouer l'interface (Slint prend son défaut) ; sans affichage (`WAYLAND_DISPLAY` et `DISPLAY` qui ne mènent nulle part), elle sort en 1.
- `flatpak remote-ls` ne montre que la branche courante de chaque runtime, et préfixe chaque ref de son genre (`runtime/...`) : les anciennes branches demandent `--all`.
- pkexec lancé par SSH n'a pas d'agent d'authentification : seul un refus ou un « sans mot de passe » de polkit y répond. La fenêtre de mot de passe ne s'ouvre que pour l'interface, lancée dans la session graphique.
