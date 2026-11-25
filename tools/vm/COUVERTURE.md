# Matrice de couverture e2e du banc VM

Inventaire de ce qu'Okapi fait, à éprouver de bout en bout dans le banc (`README.md`), tiré du code au commit `cf85078` (2026-11-25). Il sert à découper le travail en lots, une session par lot ; la couverture mesurée par instrumentation dira ce qu'il a oublié, et on l'ajoute ici sous un nouvel identifiant.

## Comment lire

- **Identifiant** : `DOM-NNN`, stable. Jamais renuméroté ; une entrée retirée reste, marquée MORTE. Une entrée ajoutée prend le numéro suivant de son domaine.
- **Montage** : préparation → actions → preuve attendue. « Ui » = pilotage AT-SPI, « ssh » = commande en `tester` (`bench.user`) ou root (`bench.root`), « horloge » = `bench.advance_clock`. Les noms entre guillemets sont les textes accessibles français.
- **Scénario** : fichier `scenarios/<domaine>_<sujet>.py` qui porte l'entrée ; un scénario porte plusieurs entrées, chacune vérifiée par un `bench.check` à elle (le message de l'échec cite l'identifiant).
- **VM** : `oui`, `partiel` (ce qui manque est dit), ou `non` (pourquoi, et quel test non-e2e la couvre).
- **Preuve** : toujours sur le système (fichier présent ou parti, `history.jsonl`, journal `okapi-app.log`, réponse JSON du helper) en plus de l'écran. Un texte affiché seul ne prouve pas qu'un fichier est resté.

Domaines : `NAV` fenêtre et barre latérale · `ACC` accueil · `CLN` nettoyeur · `ADM` droits administrateur et helper · `RGL` règles · `INV` invariants · `RGG` réglages · `HIS` historique · `DBL` doublons · `GRF` gros fichiers · `DSK` espace disque · `APP` applications · `RST` restes et copies de snapd · `OLD` anciennes versions · `MAJ` mises à jour · `DCK` Docker · `CLI` ligne de commande · `LNG` langue · `CLV` clavier et accessibilité.

## Ce qui manque au banc (bibliothèque commune)

Le banc de `bdae0b3` ne sait que cliquer, attendre, lancer des commandes et avancer l'horloge. Ces besoins sont communs à plusieurs lots : ils vont dans la bibliothèque commune (`okapivm/`, `guest/`), écrite par la session du banc parallèle, jamais dans un scénario. Un lot qui en a besoin avant qu'elle existe l'ajoute à la bibliothèque, en générique, dans un commit à part.

| Id | Besoin | Pour | Piste |
|---|---|---|---|
| BIB-01 | Touches clavier (Échap, Tab, Entrée, Espace, flèches, Suppr, Ctrl+Z) | CLV, ACC, NAV, modales | QMP `send-key` (Wayland ne prend pas `Atspi.generate_keyboard_event` partout) |
| BIB-02 | Clic souris à des coordonnées (hors d'une modale) | CLV-002 | QMP `input-send-event` en absolu, coordonnées tirées de `Component.get_extents` |
| BIB-03 | Saisie dans un champ | RGG, APP (recherche) | `EditableText.set_text_contents`, sinon frappe QMP |
| BIB-04 | Lancer l'appli avec un environnement (`LANG`, `LANGUAGE`, `TMPDIR`) et lire son code de sortie | LNG, INV-6, NAV-010 | `systemd-run --user --setenv=` |
| BIB-05 | Code de sortie, stdout et stderr séparés d'une commande ssh | CLI, ADM-02x | `bench.user(..., check=False)` rend aujourd'hui la seule sortie |
| BIB-06 | Lire et écrire les fichiers du store (`settings.json`, `history.jsonl`, `selections.json`, `interface.json`, `dashboard.json`, `last-analysis.json`) | presque tous | fonctions qui lisent le JSON par ssh |
| BIB-07 | Polkit refusé, polkit qui demande le mot de passe, fenêtre de mot de passe de GNOME fermée (pkexec 126) | ADM | règle polkit `NO`, et clic « Annuler » dans gnome-shell par AT-SPI |
| BIB-08 | Commandes en `autre` (second compte) | INV-1, INV-7, DBL | `sudo -u autre` en root |
| BIB-09 | Rouvrir la session graphique de `tester` (groupe docker, `pam_tmpdir`) | DCK, INV-6 | `loginctl terminate-user` puis connexion automatique de gdm |
| BIB-10 | Fabriques : `.deb` local avec lanceur, snap local (`snap pack`, `--dangerous`), dépôt apt local signé | APP, RST, OLD, MAJ | `dpkg-deb`, `dpkg-scanpackages`, `gpg` dans la VM |
| BIB-11 | Montages de test : boucle FAT, bind, FUSE (`bindfs`) | INV-3, INV-6, INV-7, RGG | `losetup`, `mkfs.vfat`, `mount --bind`, `bindfs` |
| BIB-12 | Lire la valeur d'une case, la description et l'état `enabled` d'un nœud | partout | `bench.ui.find` rend déjà les états ; ajouter description |
| BIB-13 | Planter un faux navigateur ouvert : `parent.lock` tenu par `fcntl` (vu dans `/proc/locks`), `SingletonLock` vers `<hôte>-<pid>` d'un processus vivant | RGL, CLN | petit script Python lancé en `tester` |

## Inventaire

### NAV : fenêtre et barre latérale

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| NAV-001 | La page de départ est « Accueil » | lancer → la page « Accueil » a la description « page affichée » | `nav_sidebar` | oui |
| NAV-002 | Chaque entrée ouvre sa page : « Accueil », « Nettoyeur », « Espace disque », « Applications », « Mises à jour », « Doublons », « Gros fichiers », « Historique », « Réglages », « À propos » | cliquer chacune → titre de la page affiché, description « page affichée » | `nav_sidebar` | oui |
| NAV-003 | Un groupe (« Nettoyage », « Logiciels », « Stockage ») se ferme et s'ouvre ; fermé sur la page affichée, il dit « contient la page affichée » | clic sur le titre → état `expanded` change, ses entrées disparaissent | `nav_sidebar` | oui |
| NAV-004 | « Replier la barre latérale » / « Déplier la barre latérale » | clic → libellés absents, icônes seules | `nav_sidebar` | oui |
| NAV-005 | Repli et groupes fermés persistent | replier, fermer « Stockage », relancer → même état ; `interface.json` dit `sidebarFolded`, `closedGroups` | `nav_sidebar` | oui |
| NAV-006 | `interface.json` illisible : barre dépliée, groupes ouverts, fichier jamais réécrit | ssh : écrire `{` → lancer, replier → fichier toujours `{` (somme identique) | `nav_sidebar` | oui |
| NAV-007 | Fenêtre étroite : barre repliée d'office, sans bouton de repli, choix gardé mais pas réécrit | redimensionner la fenêtre (gsettings ou `wmctrl` sous XWayland impossible : redimensionner par raccourci GNOME) → pas de bouton | `nav_sidebar` | partiel : le redimensionnement sous Wayland demande BIB-01 (Super+flèches) ; le seuil est testé par les tests Slint de l'appli |
| NAV-008 | Fermer pendant une analyse : la fenêtre se ferme, analyse annulée | lancer une analyse sur un gros `~/.cache` planté → fermer (BIB-01 Ctrl+Q ou bouton de fenêtre) → processus parti en moins de 2 s | `nav_closing` | oui |
| NAV-009 | Fermer pendant un nettoyage : 1re fermeture annule et garde la fenêtre (« Annulation du nettoyage »), elle se ferme seule après l'historique ; 2e fermeture ferme | nettoyage de milliers de petits fichiers vieillis → fermer → titre « Annulation du nettoyage », puis fenêtre partie, ligne d'historique écrite ; refaire avec deux fermetures → fenêtre partie aussitôt | `nav_closing` | oui |
| NAV-010 | Lancée en root, l'appli refuse de démarrer | ssh root : lancer `okapi-app` → aucune fenêtre, « refusing to run as root » sur stderr | `nav_closing` | oui (BIB-04/05) |
| NAV-011 | Fermer pendant une désinstallation : deux fermetures nécessaires, message « Une désinstallation est en cours. Fermez encore Okapi… » | voir APP-012 | `apps_uninstall_deb` | oui |
| NAV-012 | Fermer pendant une mise à la corbeille : attente jusqu'à 5 s qu'elle finisse | doublons à mettre à la corbeille sur un gros fichier → fermer pendant le déplacement → fichier entier soit dans la corbeille soit à sa place, jamais à moitié | `nav_closing` | partiel : la fenêtre de 5 s est courte, la course n'est pas garantie |
| NAV-013 | « À propos » : version, licence, zone « Licences » défilante | ouvrir → « Version {} » égale à `okapi --version` | `nav_sidebar` | oui |

### ACC : accueil (tableau de bord et moniteur)

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| ACC-001 | Disposition par défaut : Récupérable, Disques, Mémoire, Processeur, Système ; pas « Mises à jour » | `dashboard.json` absent → lancer → cinq widgets dans cet ordre | `home_layout` | oui |
| ACC-002 | « Personnaliser » → « Terminé », bandeau du mode, « Ajouter un widget », « Rétablir la disposition par défaut » | clic → nœuds présents ; « Terminé » → disparus | `home_layout` | oui |
| ACC-003 | Changer la taille d'un widget (« Taille de {titre} ») et la retrouver après relance | « Mémoire » en « Large » → `dashboard.json` dit `wide` ; relancer → toujours Large | `home_layout` | oui |
| ACC-004 | Déplacer au clavier : poignée « Déplacer {titre} », Entrée, flèches, Début/Fin, Entrée ; Échap remet en place | BIB-01 → description « Place n sur total » change ; Échap → place d'origine | `home_layout` | oui (BIB-01) |
| ACC-005 | Déplacer à la souris vers « Déposer ici » | glisser QMP → ordre changé | `home_layout` | partiel : glisser en QMP fragile ; le clavier (ACC-004) porte le même modèle |
| ACC-006 | Retirer un widget : toast « {titre} retiré du tableau de bord », « Annuler » le remet, Ctrl+Z aussi, toast parti après 8 s | retirer « Système » → toast ; « Annuler » → revenu ; retirer encore, attendre 9 s → toast parti, widget absent de `dashboard.json` | `home_layout` | oui |
| ACC-007 | Modale « Ajouter un widget » : une ligne par widget absent, « Ajouter {titre} », état vide « Tous les widgets sont déjà sur votre tableau de bord. », « Fermer » | ajouter « Mises à jour » → présent ; rouvrir avec tous présents → état vide | `home_layout` | oui |
| ACC-008 | « Rétablir la disposition par défaut ? » : « Annuler » ne change rien, « Rétablir » remet la disposition | modifier, rétablir → `dashboard.json` égal au défaut | `home_layout` | oui |
| ACC-009 | Tableau vide : « Votre tableau de bord est vide » et son bouton « Ajouter un widget » | retirer les cinq → état vide | `home_layout` | oui |
| ACC-010 | `dashboard.json` illisible : défaut affiché, changements non enregistrés, fichier intact ; entrée incomprise écartée | ssh : `{` puis une entrée `kind` inconnue → bandeau « Votre disposition n'a pas pu être lue… », fichier inchangé | `home_layout` | oui |
| ACC-011 | Récupérable : « Aucune analyse », bouton « Analyser » qui coche le défaut, ouvre le Nettoyeur et analyse ; après analyse la taille, « Estimation », puis « {} libérés » après un nettoyage | sans `last-analysis.json` → « Aucune analyse » ; « Analyser » → page Nettoyeur en analyse ; après nettoyage → « libérés » | `home_recoverable` | oui |
| ACC-012 | `last-analysis.json` périmé (historique plus récent) et illisible | ssh : fichier corrompu → « Le résumé de la dernière analyse n'a pas pu être lu… » | `home_recoverable` | oui |
| ACC-013 | « Analyser » de Récupérable désactivé pendant une tâche, une demande de droits, une confirmation, en mode Personnaliser | état `enabled` faux dans chaque cas | `home_recoverable` | oui |
| ACC-014 | Disques : « {} libres sur {} » par volume, « Presque plein », « amovible », « partage réseau », « Ne répond pas » | ssh root : boucle FAT montée (amovible si `udisksctl`), volume rempli à 95 % (`fallocate`), montage NFS local figé (`kill -STOP` du serveur) → libellés | `home_monitor` | partiel : « Ne répond pas » demande un NFS local (`nfs-kernel-server` à provisionner) |
| ACC-015 | Mémoire : occupé, swap, « Pas de swap », « Applications qui en occupent le plus » nommées par leur lanceur | `swapoff -a` → « Pas de swap » ; lancer gnome-terminal → nommé | `home_monitor` | oui |
| ACC-016 | Processeur : pourcentage et charge moyenne cohérents avec `/proc` | `stress`-like (`yes > /dev/null`) → pourcentage monte | `home_monitor` | oui |
| ACC-017 | Système : Distribution, Noyau, Bureau, Allumé depuis | comparer à `/etc/os-release`, `uname -r` | `home_monitor` | oui |
| ACC-018 | Widget Mises à jour : « Tout est à jour », « Recherche… », « Voir les mises à jour » ouvre la page | voir MAJ ; ajouter le widget → états | `updates_check` | oui |
| ACC-019 | Sous-ligne de l'en-tête : « dernière analyse aujourd'hui à… / hier à… » | analyser, horloge +1 j → « hier à » | `home_recoverable` | oui |

### CLN : nettoyeur

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| CLN-001 | Cartes « Système », « Applications », « Navigateurs », « Jeux » cochées, « Caches GPU » décochée ; « Mes dossiers » absente sans dossier choisi | page Nettoyeur → états des cases | `cleaner_categories` | oui |
| CLN-002 | « Mes dossiers » apparaît décochée, en dernier, dès qu'un dossier est choisi | `settings.json` avec `customFolders` → carte présente, décochée | `cleaner_categories` | oui |
| CLN-003 | « Analyser » désactivé sans catégorie : « Aucune catégorie sélectionnée » | tout décocher → bouton désactivé | `cleaner_categories` | oui |
| CLN-004 | Ligne « Toujours épargnés : fichiers modifiés depuis moins de {} min. Exclusions : {}. » suit les réglages | âge 120, deux exclusions → texte | `cleaner_categories` | oui |
| CLN-005 | Analyse limitée aux catégories cochées | planter dans Système et Jeux ; ne cocher que Jeux → seuls des groupes Jeux | `cleaner_categories` | oui |
| CLN-006 | Progression : « Analyse en cours », « Catégories terminées : n sur m », lignes « En attente / En cours / Terminé », « Trouvé jusqu'ici » | gros `~/.cache` (100 000 fichiers vieillis) → états vus | `cleaner_scan` | oui |
| CLN-007 | « Annuler l'analyse » : retour au choix des catégories, rien supprimé, bouton désactivé une fois cliqué | annuler en cours → page de choix ; fichiers intacts | `cleaner_scan` | oui |
| CLN-008 | « Rien à nettoyer » (et sa variante avec remarques) | VM propre, catégorie Jeux seule → titre | `cleaner_scan` | oui |
| CLN-009 | « Rien n'a été regardé » avec ses raisons (variable absente, dossier de variable illisible, âge avant l'horloge) | lancer avec `XDG_CACHE_HOME` vers un dossier `chmod 000` (BIB-04) → raison « Le dossier de la variable {} est illisible… » ; `minAgeMinutes` énorme → raison d'horloge | `cleaner_problems` | oui |
| CLN-010 | Bandeaux bloquants : réglages illisibles, exclusion refusée, dossier à nettoyer relatif, âge avant l'horloge | `settings.json` invalide / `exclusions:["rel"]` / `customFolders:["rel"]` → bandeau, « Analyser » désactivé | `cleaner_problems` | oui |
| CLN-011 | Écran de résultats : « Résultats », « Trouvé : {}. Éléments : {}. », arbre catégorie → navigateur → groupe, « Déplier / Replier » | analyse avec Système, Navigateurs (profil Chromium planté), Applis → arbre | `cleaner_results` | oui |
| CLN-012 | Tri par « Élément », « Éléments », « Taille », sens inversé au second clic, nom « trié par ordre décroissant » | clics → ordre et noms | `cleaner_results` | oui |
| CLN-013 | Descriptions de groupe : « Présélectionné », « Administrateur », « sélection partielle », « À choisir groupe par groupe » | lire les descriptions (BIB-12) | `cleaner_results` | oui |
| CLN-014 | Case de catégorie : décoche tout, puis ne coche que les présélectionnés | Navigateurs avec « Cache » (coché) et « Cache GPU » (décoché) → décocher puis cocher la catégorie → GPU toujours décoché | `cleaner_results` | oui |
| CLN-015 | « Pourquoi c'est sans risque » / « Ce qui part » se déplient | clic → texte | `cleaner_results` | oui |
| CLN-016 | Remarques : « Afficher / Masquer », « Dossier exclu par vos réglages : {} », « Dossier illisible, non compté : {} » | exclure un dossier visé, `chmod 000` un autre → lignes | `cleaner_results` | oui |
| CLN-017 | « Tout désélectionner » ne vaut que pour cette analyse ; « Nettoyer » désactivé sans rien de coché | clic → tout décoché ; `selections.json` inchangé ; nouvelle analyse → défaut | `cleaner_kept_choices` | oui |
| CLN-018 | Une case décochée est retenue (`selections.json`) et repart décochée ; un choix retenu ne coche jamais ; un groupe admin n'est jamais retenu | décocher « Cache des miniatures » → analyse suivante décochée ; cocher « Cache des polices » (à choisir) → analyse suivante décochée | `cleaner_kept_choices` | oui |
| CLN-019 | `selections.json` illisible : bandeau, cases par défaut, fichier intact | ssh : `{` → bandeau « Vos choix précédents n'ont pas pu être lus… » | `cleaner_kept_choices` | oui |
| CLN-020 | « Retour » revient au choix ; « Nouvelle analyse » relance | clics | `cleaner_results` | oui |
| CLN-021 | Confirmation « Supprimer définitivement ? » : taille et nombre justes, case « J'ai compris que c'est définitif » décochée à chaque ouverture, « Supprimer définitivement » désactivé sans elle | ouvrir, fermer par « Annuler », rouvrir → case décochée ; bouton `enabled` faux | `cleaner_confirm` | oui |
| CLN-022 | « Annuler » et Échap de la confirmation : rien supprimé | fichiers plantés toujours là | `cleaner_confirm` | oui (BIB-01) |
| CLN-023 | Phrases ajoutées selon le cas : corbeille, caches recréés, Docker, mot de passe | cocher « Corbeille » → « Ce qui part de la corbeille ne pourra plus être restauré. » | `cleaner_confirm` | oui |
| CLN-024 | Un double-clic sur « Nettoyer » ne traverse pas la modale (délai `settle`) | double-clic rapide puis clic à l'endroit du bouton danger → modale toujours ouverte, rien supprimé | `cleaner_confirm` | partiel : demande BIB-02 au bon tempo |
| CLN-025 | Nettoyage en cours : étapes « Vérification de chaque élément », « Suppression des éléments vérifiés », « Enregistrement dans l'historique », « Libéré jusqu'ici » | gros nettoyage → états | `cleaner_clean` | oui |
| CLN-026 | « Annuler le nettoyage » : « Nettoyage annulé », ce qui n'était pas retiré reste, historique écrit | annuler au milieu de 50 000 fichiers → reçu « Nettoyage annulé », fichiers restants comptés égaux à « Gardées » | `cleaner_clean` | oui |
| CLN-027 | Reçu : « Nettoyage terminé », chiffres « Espace libéré », « Retirées », « Déjà parties », « Gardées », « Détail par résultat » et raisons | planter un fichier qui part, un rajeuni (INV-2), un exclu → raisons « Gardées : modifiées trop récemment », « exclues par vos réglages » | `cleaner_clean` | oui |
| CLN-028 | « Déjà parties » : un fichier retiré entre analyse et nettoyage | ssh `rm` entre les deux → compté « Déjà parties » | `cleaner_clean` | oui |
| CLN-029 | « Le reçu est enregistré dans l'historique. » et la ligne de `history.jsonl` égale au reçu | comparer octets et comptes | `cleaner_clean` | oui |
| CLN-030 | Historique indisponible avec journal des suppressions demandé : rien supprimé, bandeau | `deletionLog:true`, `history.jsonl` remplacé par un dossier → « L'historique ne peut pas être tenu… rien n'est supprimé » ; fichiers là | `cleaner_problems` | oui |
| CLN-031 | Historique indisponible sans journal des suppressions : nettoyage fait, « L'historique est indisponible… » | idem `deletionLog:false` → fichiers partis, texte | `cleaner_problems` | oui |
| CLN-032 | « Terminer » revient au choix ; Accueil et Historique mis à jour | après reçu → page de choix, Récupérable « libérés » | `cleaner_clean` | oui |
| CLN-033 | Question navigateur « {nav} est ouvert » : « Ignorer {nav} » garde ses caches, le reste part | BIB-13 Firefox factice ouvert, cache `cache2` vieilli + miniatures → « Ignorer Firefox » → `cache2` là, miniatures parties, raison « leur navigateur est ouvert » | `cleaner_browsers` | oui |
| CLN-034 | « Fermer {nav} et continuer » : signal au pid vérifié, attente, puis nettoyage | processus factice qui sort sur SIGTERM → caches partis | `cleaner_browsers` | oui |
| CLN-035 | Navigateur qui ne se ferme pas : « {nav} ne s'est pas fermé : fermez-le… » ; rien de son cache supprimé | processus qui ignore SIGTERM → message après ~15 s | `cleaner_browsers` | oui |
| CLN-036 | « Annuler » (ou Échap) de la question navigateur annule tout le nettoyage | rien supprimé | `cleaner_browsers` | oui |
| CLN-037 | Ignorer le seul navigateur coché : retour aux résultats | ne cocher que son cache → « Ignorer » → page Résultats | `cleaner_browsers` | oui |
| CLN-038 | Verrou non vérifiable (flatpak, autre espace de pid) : jamais de signal, « Okapi ne peut pas fermer {nav} lui-même » | `SingletonLock` d'un Chromium flatpak factice → message, aucun processus tué | `cleaner_browsers` | oui |
| CLN-039 | Réglages changés pendant que les résultats sont affichés : relus au retour sur la page, un dossier retiré dit `no_longer_chosen` | analyser « Mes dossiers », retirer le dossier dans Réglages, nettoyer → gardé, raison « n'est plus choisi » | `cleaner_problems` | oui |

### ADM : droits administrateur, pkexec et helper

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| ADM-001 | Parcours complet : « Obtenir les droits », « Droits obtenus », groupes admin offerts décochés, nettoyage, reçu | modèle existant | `cleaner_admin` (existe) | oui |
| ADM-002 | Bandeau « Groupes qui demandent les droits administrateur : {}. » liste les bons groupes | planter dans `/var/crash`, `/var/log/apt` → noms | `admin_rights` | oui |
| ADM-003 | Étapes « Préparation… », « Saisissez le mot de passe dans la fenêtre du système… », « Analyse avec les droits administrateur… » et « Annuler » visible | polkit avec mot de passe (BIB-07) → texte pendant la fenêtre | `admin_rights` | oui |
| ADM-004 | Fenêtre de mot de passe fermée : « L'authentification a été annulée. », groupes toujours demandés | BIB-07 clic « Annuler » de gnome-shell → texte ; rien dans `/var/crash` touché | `admin_rights` | oui (BIB-07) |
| ADM-005 | Polkit refuse : « Les droits administrateur ont été refusés… » | règle polkit `NO` → texte | `admin_rights` | oui |
| ADM-006 | « Annuler » avant la fenêtre et pendant : « Annulation de la demande des droits… » / « …une fois la fenêtre du système fermée. » | annuler pendant la fenêtre, puis la fermer → aucun scan root dans le journal du helper | `admin_rights` | oui |
| ADM-007 | Helper absent : « L'assistant administrateur n'est pas installé… », pkexec pas lancé | ssh root : retirer `/usr/libexec/okapi/okapi-helper` → texte ; `journal.txt` sans pkexec | `admin_helper_install` | oui |
| ADM-008 | Helper d'une autre version : « …n'est pas à jour : relancez packaging/linux/install-helper.sh. » | remplacer par un script root qui répond `refused: unsupportedVersion` (binaire d'un commit antérieur à la version 5 du protocole) | `admin_helper_install` | partiel : demande un helper d'une autre version, construit d'un vieux commit |
| ADM-009 | Réponse illisible : « L'assistant administrateur a échoué. » | helper remplacé par un binaire qui écrit `x` (root, même chemin) → texte | `admin_helper_install` | oui |
| ADM-010 | Réglages changés depuis les droits : rien supprimé, bandeau « Les réglages ont changé depuis l'obtention des droits… » | droits obtenus, puis ajouter une exclusion, nettoyer → rien parti en `/var/crash` | `admin_rights` | oui |
| ADM-011 | Partie admin pendant le nettoyage : étape « Nettoyage des éléments administrateur », `admin.done` dans l'historique | `history.jsonl` : `admin:{done:true}` | `admin_rights` | oui |
| ADM-012 | Ce qui a changé entre droits et nettoyage : « Une partie de ce que l'assistant administrateur devait nettoyer a changé… » | droits obtenus, puis ssh root retire `/var/crash` et le recrée (autre identité) → texte, rien retiré dans le nouveau dossier | `admin_rights` | oui |
| ADM-013 | Réglage « Opérations administrateur » coupé : aucun groupe admin à l'analyse suivante, pas de bandeau de droits | réglage off → analyse → aucun groupe « Administrateur » | `admin_setting_off` | oui |
| ADM-014 | Une ligne que root seul lit (« Illisible sans les droits administrateur ») attend les droits, jamais cochée ni avant ni après | `/var/lib/systemd/coredump` illisible pour `tester` → ligne non cochée après droits | `admin_rights` | oui |
| ADM-015 | Vacuum du journal : ligne « Journaux de systemd » jamais cochée, mesurée par root ; cochée, les archives de plus de 30 j partent, l'actif et les récentes restent | ssh root : archives factices nommées `system@<seqid>-<seq>-<rt>.journal` dans `/var/log/journal/<machine-id>`, horloge +31 j, nouvelle archive récente → seules les vieilles parties | `admin_journal` | partiel : à confirmer au montage si le plan lit l'en-tête (sinon fabriquer de vraies archives par `journalctl --rotate` avant l'horloge) |
| ADM-016 | Vacuum à la taille : au-delà de 1 Go, les plus vieilles partent ; une archive verrouillée (`chattr +i`) retient la règle de taille | archives totalisant 1,2 Go, la plus vieille `+i` → aucune plus récente partie, « gardée » dite | `admin_journal` | oui |
| ADM-017 | Vacuum : `/etc/machine-id` illisible ou absent → rien ne part | `mv /etc/machine-id` → issue `unreadableRoot` | `admin_journal` | oui |
| ADM-018 | Protocole du helper : version 5 seule, cibles de la liste seules, `/var/crash/` refusé, doublon refusé, âge 59 refusé et 60 accepté, exclusion relative refusée, requête de 64 Kio + 1 refusée, liste vide refusée ; chaque refus en entier, exit 2, rien touché | ssh root : `echo '<json>' \| /usr/libexec/okapi/okapi-helper` pour chaque cas → `refused` attendu, fichiers plantés là | `admin_helper_protocol` | oui (BIB-05) |
| ADM-019 | Helper lancé sans être root : `NotRoot`, exit 3 | ssh tester : lancer le binaire → exit 3 | `admin_helper_protocol` | oui |
| ADM-020 | Le helper n'écrit aucun fichier à lui ; ses traces JSON arrivent dans `okapi-app.log` | `find / -newer` après un nettoyage admin → rien d'autre que les suppressions ; journal de l'appli contient « request accepted » | `admin_rights` | oui |
| ADM-021 | `auth_admin_keep` : un second pkexec dans les minutes suivantes ne redemande pas le mot de passe | polkit avec mot de passe : droits, puis nettoyage → une seule fenêtre | `admin_rights` | partiel : demande BIB-07 en mode mot de passe |
| ADM-022 | Cache de Snap (`keepHardLinked`) : seuls les fichiers que nul snap installé ne partage partent | snap local installé (BIB-10), fichier de cache lié dur à son `.snap` + un orphelin → seul l'orphelin part | `admin_rights` | oui (BIB-10) |
| ADM-023 | `install-helper.sh` refuse root et installe politique, dossier et binaire aux bons modes | hors VM : script lancé par le banc lui-même | — | non : le banc installe déjà par ses propres chemins ; script relu à la main, pas de test |

### RGL : règles de nettoyage

Les 320 entrées de règles Linux (`crates/rules/data/linux/`) se regroupent en familles ; un scénario par famille plante un échantillon de chaque cas particulier, pas les 320 chemins. Un test non-e2e de `okapi-rules` valide déjà chaque fichier (`rule_set.rs`).

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| RGL-001 | Système, compte : `${TMPDIR}`, `/tmp`, `/var/tmp` à 30 j, miniatures, Tracker (deux), GStreamer, `.xsession-errors.old` | planter chacun, horloge +31 j → chaque groupe coché et vidé | `rules_system` | oui |
| RGL-002 | Temporaires à 30 j : un fichier de 29 j reste | horloge +29 j → absent des résultats | `rules_system` | oui |
| RGL-003 | Système, admin : `/var/crash`, `/var/cache/apt/archives`, `/var/lib/flatpak/repo/tmp`, `/var/lib/systemd/coredump`, `/var/log/installer`, `/var/cache/man`, `/var/log/apt` | planter en root, droits, nettoyer → partis | `rules_system_admin` | oui |
| RGL-004 | DNF, Pacman, Zypper absents d'Ubuntu : aucune ligne | analyse → noms absents | `rules_system_admin` | oui |
| RGL-005 | Caches de polices (compte et système) : `cacheReset`, décochés, « À choisir » | planter → décochés | `rules_system` | oui |
| RGL-006 | Caches snap et flatpak des applis (`~/snap/*/common/.cache`, `~/.var/app/*/cache`) : `childSubdir` | deux applis factices → deux entrées | `rules_system` | oui |
| RGL-007 | Corbeille : décochée, « Supprimé définitivement », une entrée part entière d'après son `.trashinfo` de plus de 30 j + 1 j de marge | `gio trash` de fichiers, horloge +32 j → entrée et `.trashinfo` partis ; une de 30 j reste | `rules_trash` | oui |
| RGL-008 | Corbeille : `.trashinfo` réécrit entre analyse et nettoyage (date récente) garde son entrée | ssh : réécrire `DeletionDate` → gardée | `rules_trash` | oui |
| RGL-009 | Corbeille : `.trashinfo` lien, trop gros (> 32 Kio), sans date, d'un autre propriétaire → entrée gardée | quatre entrées piégées → toutes gardées | `rules_trash` | oui |
| RGL-010 | Corbeille : une entrée restaurée entre analyse et nettoyage est « Déjà partie », son nouveau `.trashinfo` reste | `gio trash --restore` entre les deux | `rules_trash` | oui |
| RGL-011 | Navigateurs Chromium : chaque type de cache par profil et partagé, cochés ou non selon la table, libellé « {Navigateur} : {libellé} » | profils factices Chrome et Brave snap (`Default`, `Profile 1`) → groupes et cases | `rules_browsers` | oui |
| RGL-012 | Firefox natif, snap, flatpak : `cache2`, `thumbnails` cochés, `startupCache` décoché | profils factices avec `parent.lock` libre | `rules_browsers` | oui |
| RGL-013 | Firefox ouvert : verrou lu dans `/proc/locks` par inode ; profil sans `parent.lock` ignoré | BIB-13 → question navigateur ; profil sans verrou → pas de groupe | `rules_browsers` | oui |
| RGL-014 | Chromium `SingletonLock` vers un pid mort : jugé fermé | lien vers `hôte-99999` → pas de question | `rules_browsers` | oui |
| RGL-015 | Applis Electron (Discord, VS Code, Slack...) : chemins « std », « +lg », « +cd », « +vsix » | trois applis factices → groupes | `rules_apps` | oui |
| RGL-016 | `minAgeDays` des applis (1 j Cursor, 7 j Figma) | horloge +2 h → Cursor absent ; +2 j → présent, Figma absent | `rules_apps` | oui |
| RGL-017 | Outils de dev : npm, pip, Cargo, Gradle, Maven, gems (`childSubdir`), Conda | échantillon → groupes | `rules_apps` | oui |
| RGL-018 | uv et pnpm « À faire vous-même » : jamais cochés, commande `uv cache prune` / `pnpm store prune` dans « Commande à copier », « Copier », rien lancé | `~/.cache/uv` planté → note, presse-papiers (`wl-paste`) égal à la commande, aucun processus `uv` | `rules_apps` | oui |
| RGL-019 | Hugging Face Xet : `cacheReset`, 7 j, décochée | planter → décochée | `rules_apps` | oui |
| RGL-020 | Jeux : Steam (journaux, httpcache), itch.io, Heroic, Lutris, Minecraft cochés ; « Précache des shaders Steam » décoché | planter → cases | `rules_gaming_gpu` | oui |
| RGL-021 | GPU : Mesa, NVIDIA, CUDA, Unity : catégorie « À choisir », tous décochés | planter → carte « Caches GPU » décochée, groupes décochés | `rules_gaming_gpu` | oui |
| RGL-022 | Racine partagée `${CACHE}/unity3d` (Applis cochée, GPU `cacheReset`) : parcourue une fois, jamais présélectionnée | planter → un groupe, décoché (invariant 5) | `rules_gaming_gpu` | oui |
| RGL-023 | `databases.json` et `steam.json` ne produisent aucune ligne | — | — | non : aucune règle d'analyse n'en sort aujourd'hui ; validées par les tests d'`okapi-rules` |
| RGL-024 | Une règle fournie par l'utilisateur n'existe pas : les règles sont embarquées | — | — | non : rien à éprouver en VM ; `deny_unknown_fields` et `CleanupAction` fermée testés (`rejects_unknown_fields_*`) |

### INV : les sept invariants, attaqués

Chaque attaque se monte entre l'analyse et le nettoyage (le moment que le code revérifie), puis la preuve se lit sur le disque. Une course pendant la passe de retrait n'est pas déterministe en VM : elle est dite `non`, avec le test qui la garde.

**Invariant 1 : identifiants, jamais chemins ; root et les dossiers des autres**

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| INV-101 | Root n'entre jamais dans un dossier qui n'est ni à root ni au propriétaire de la racine : arbre de `autre` posé dans `/var/crash` | BIB-08 : `autre` crée `/var/crash/arbre/{a,b}` vieillis → droits, cocher, nettoyer → arbre intact ; fichiers de `autre` directement sous `/var/crash` offerts | `inv_root_owners` | oui |
| INV-102 | Un dossier qui change de propriétaire entre droits et nettoyage n'est pas vidé | droits obtenus, puis `chown -R autre /var/crash/vieux` → gardé | `inv_root_owners` | oui |
| INV-103 | Le helper refait l'analyse : un fichier planté après les droits n'est pas offert sous un nom qu'il ne connaît pas, et un fichier changé reste | droits, ajouter un fichier neuf en `/var/crash` → absent du reçu, toujours là | `inv_root_owners` | oui |
| INV-104 | Le helper ne reçoit que des chemins modèles : une requête qui nomme un fichier (`/var/crash/x.crash`) est refusée en entier | ADM-018, cas `unknownTarget` | `admin_helper_protocol` | oui |
| INV-105 | Un groupe partagé avec une règle non demandée est gardé (`withheld`) | requête root à la main qui demande une seule des règles d'une racine partagée | `admin_helper_protocol` | partiel : aucune racine admin n'est partagée sur Linux (validation de `rule_set.rs`), le cas n'existe que dans les tests d'`okapi-helper` |
| INV-106 | La corbeille n'est jamais l'affaire de root | — | — | non : aucune cible `trash` admin ne passe la validation ; `root_never_empties_a_trash` |
| INV-107 | Docker : seule la socket trouvée à l'analyse est utilisée, jamais un groupe du rapport | voir DCK-004 | `docker_prune` | oui |
| INV-108 | Un identifiant d'un autre rapport ne touche rien | — | — | non : l'interface ne laisse passer aucun identifiant étranger ; `an_identifier_of_another_report_touches_nothing…` |

**Invariant 2 : âge revérifié jusqu'aux descendants**

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| INV-201 | Un fichier rajeuni entre analyse et nettoyage reste (`Recent`) | horloge +2 h, analyser, ssh `echo >> fichier` → gardé, raison « modifiées trop récemment » | `inv_age` | oui |
| INV-202 | `touch -d` vers le passé ne vieillit pas : la date de changement d'inode compte | fichier neuf `touch -d '2 years ago'` → absent des résultats | `inv_age` | oui |
| INV-203 | Un descendant profond rajeuni garde son dossier entier | `~/.cache/thumbnails/large/a/b/c`, vieilli ; analyser ; écrire dans `c` → dossier `large` entier là ; côté root, le helper refait l'analyse au nettoyage : le descendant rajeuni reste (« held back », `Recent`), ses vieux voisins partent un à un | `inv_age` | oui |
| INV-204 | Un dossier changé à l'instant (extraction, verrou `mkdir`) n'est pas offert, ses vieilles entrées si | dossier vieilli puis `mkdir verrou` dedans avant l'analyse → entrées offertes, dossier non | `inv_age` | oui |
| INV-205 | Liens physiques : un fichier lié ne rend pas l'autre récent | deux noms d'un même inode vieilli → les deux partent | `inv_age` | oui |
| INV-206 | Root : âge sous le plancher de 1 h refusé même si les réglages disent 30 min | `minAgeMinutes:30`, fichier admin de 45 min → helper ne l'offre pas, aucune erreur | `inv_age` | oui |
| INV-207 | Vacuum : une archive changée après le plancher reste ; une trop jeune au nettoyage reste | droits, puis `touch` d'une archive (ctime) → gardée | `admin_journal` | oui |
| INV-208 | Corbeille : l'âge de l'utilisateur au nettoyage vaut pour la corbeille aussi | âge 24 h, entrée de 31 j → part ; RGL-008 pour la relecture | `rules_trash` | oui |
| INV-209 | Fichier rajeuni pendant la passe de retrait elle-même | — | — | non : course, `tests::clean::the_removal_judges_each_entry_again` |

**Invariant 3 : liens jamais suivis, exclusions par chemin réel**

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| INV-301 | Un lien symbolique planté dans une cible vers `~/Documents` n'est ni suivi ni retiré ; la cible du lien est intacte | `~/.cache/thumbnails/piege -> ~/Documents`, vieilli → analyse sans `Documents` ; nettoyage → `Documents` intact, lien gardé | `inv_links` | oui |
| INV-302 | Un ancêtre de la cible remplacé par un lien entre analyse et nettoyage | analyser, puis `mv ~/.cache/thumbnails t2; ln -s ~/Documents ~/.cache/thumbnails` → `Documents` intact, « Replaced » | `inv_links` | oui |
| INV-303 | Racine qui est un lien : refusée (`unsafeRoot`) | `~/.cache/gstreamer-1.0 -> ~/Documents` → remarque, rien offert | `inv_links` | oui |
| INV-304 | Root : un lien planté par `tester` sous une cible admin (`/var/tmp` ou `/var/crash` ouverts à tous) vers `/etc` | `ln -s /etc /var/crash/piege`, vieilli → `/etc` intact après nettoyage admin | `inv_links` | oui |
| INV-305 | Un montage posé dans une cible depuis l'analyse n'est jamais entré | analyser, puis `mount --bind ~/Documents ~/.cache/thumbnails/sous` (root) → `Documents` intact | `inv_links` | oui (BIB-11) |
| INV-306 | Une exclusion par lien : l'interface envoie le chemin réel, root garde ce que l'utilisateur exclut | exclusion `~/lien-crash` où `~/lien-crash -> /var/crash/garde` → `/var/crash/garde` intact après nettoyage admin | `inv_links` | oui |
| INV-307 | Le helper ne lit jamais une exclusion : un lien vivant et un lien cassé donnent la même réponse, les traces ne disent pas où mène le lien | requête root à la main avec chacune → réponses égales, `okapi-app.log` sans la cible | `admin_helper_protocol` | oui |
| INV-308 | Exclusion couverte par identité (bind) : la cible entière est gardée | exclure `~/miroir` monté bind sur `~/.cache/thumbnails` → rien de `thumbnails` retiré | `inv_links` | oui (BIB-11) |
| INV-309 | Socket, FIFO, périphérique dans une cible : ni offert ni retiré | `mkfifo`, socket Python dans `~/.cache/tracker3` → gardés | `inv_links` | oui |
| INV-310 | Permutation entre deux lectures par chemin pendant le scan | — | — | non : course, nettoyage par descripteurs gardé par `an_ancestor_swapped_for_a_link_is_never_followed` |

**Invariant 4 : une règle est de la donnée, aucune commande**

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| INV-401 | Okapi ne lance aucune commande hormis pkexec sur le helper, programme et arguments fixes | `auditd` (ou `bpftrace` sur `execve`) pendant un scénario complet du nettoyeur → seuls `pkexec --disable-internal-agent /usr/libexec/okapi/okapi-helper` et le helper | `inv_no_command` | oui (`auditd` à provisionner) |
| INV-402 | Le helper ne lance rien, même pour le vacuum (aucun `journalctl`) | même trace pendant ADM-015 | `inv_no_command` | oui |
| INV-403 | uv, pnpm : rien lancé | même trace pendant RGL-018 | `inv_no_command` | oui |
| INV-404 | Docker ne reçoit que `POST /build/prune` figé ; l'analyse ne s'y connecte pas | voir DCK-002, DCK-003 | `docker_prune` | oui |
| INV-405 | systemd de la session : seulement `ListUnitsByPatterns` et `KillUnit(scope, "all", 15 puis 9)` d'un scope de l'appli à désinstaller | `dbus-monitor --session` pendant APP-009 → appels exacts | `apps_close_scopes` | oui |
| INV-406 | Jamais un scope où tournent Okapi ou un ancêtre : Okapi lancé depuis le scope d'un gnome-terminal qu'on désinstalle | lancer Okapi depuis gnome-terminal (même scope), demander de désinstaller un `.deb` factice dont le lanceur est celui du terminal → rien fermé, message | `apps_close_scopes` | oui |
| INV-407 | Lignée inconnue : rien fermé | — | — | partiel : lignée illisible difficile à provoquer ; `okapi_platform::lineage_cgroups`, testé |
| INV-408 | snapd et flatpak : requêtes figées des anciennes versions (`{"action":"remove","revision":"N"}`, `Uninstall` puis un `PruneLocalRepo` sans `RunTriggers`) | `dbus-monitor --system` et journal de snapd pendant OLD-004/005 | `apps_old_versions` | oui |

**Invariant 5 : pas de présélection des réinitialisations ni des maintenances**

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| INV-501 | Aucune ligne `cacheReset` cochée (polices, `Code Cache`, `startupCache`, shaders, GPU) | planter un de chaque → décochés | `inv_preselection` | oui |
| INV-502 | Maintenances jamais cochées : journal, Docker, uv, pnpm, ni avant ni après les droits, ni par la case de leur catégorie | cocher la catégorie Applis → Docker toujours décoché | `inv_preselection` | oui |
| INV-503 | Corbeille, dossiers choisis (« Mes dossiers »), anciennes versions des snaps et environnements flatpak : décochés | après analyse et après un retrait (OLD) | `inv_preselection` | partiel : corbeille et « Mes dossiers » oui ; l'image de base n'a ni révision de snap désactivée ni environnement flatpak jugé inutilisé (12 jugés, 0 inutilisé), l'onglet « Anciennes versions » est vide et le scénario le trace sans rien prouver |
| INV-504 | Un choix retenu (`selections.json`) ne coche jamais ; un `selections.json` forgé qui dirait « coché » ne coche rien | ssh : écrire un `selections.json` valide qui ne liste que des décochages + une clé inconnue → aucun groupe à choisir coché | `inv_preselection` | oui |
| INV-505 | Analyse limitée à une catégorie garde l'attribut de la racine partagée | Unity, catégorie Applis seule → décoché | `rules_gaming_gpu` | oui |
| INV-506 | La ligne « À faire vous-même » n'est jamais cochable | case `enabled` faux | `rules_apps` | oui |

**Invariant 6 : jamais une racine qui contient une variable**

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| INV-601 | `TMPDIR` sous `/tmp/user/<uid>` (`pam_tmpdir`) : `/tmp` parcouru autour, jamais entré dans `/tmp/user` ni dans `TMPDIR`, sauf par la règle `${TMPDIR}` | `libpam-tmpdir` installé, session rouverte (BIB-09) ou appli lancée avec `TMPDIR=/tmp/user/1000` (BIB-04) ; vieux fichiers dans `/tmp`, `/tmp/user/1000` et `/tmp/user/1001` → `/tmp/user/1001` intact, `/tmp/user` jamais retiré | `inv_variables` | oui |
| INV-602 | `HOME` sous `/tmp` (session invitée) : `/tmp` refusé comme racine | `autre` avec `HOME=/tmp/invite` (BIB-04) → remarque `protectedRoot`, rien de `/tmp` offert | `inv_variables` | oui |
| INV-603 | `XDG_CACHE_HOME` (ou `XDG_CONFIG_HOME`) pointé sur une racine de règle : refus par identité, à travers un bind | `XDG_CACHE_HOME=/var/tmp/cache` → `/var/tmp` refusé ; même avec `/var/tmp` vu par un montage bind | `inv_variables` | oui (BIB-11) |
| INV-604 | `XDG_RUNTIME_DIR` jamais vidé, même contenu dans `TMPDIR` (cas Nix) | lancer avec `TMPDIR=/run/user` (parent du dossier de la session) → seules les règles `${TMPDIR}` écartées, le reste de l'analyse fait, `/run/user/1000` intact | `inv_variables` | oui |
| INV-605 | Une variable illisible arrête tout (« Rien n'a été regardé ») | CLN-009 | `cleaner_problems` | oui |
| INV-606 | Racine devenue ancêtre d'une variable entre analyse et nettoyage : toutes ses entrées gardées | analyser, relancer l'appli avec `XDG_CACHE_HOME` sous `/var/tmp` sans réanalyser… | — | non : changer une variable impose de relancer l'appli, donc de réanalyser ; `a_root_that_came_to_hold_a_variable_keeps_all_its_entries` |
| INV-607 | Dossier à nettoyer refusé à l'ajout et à chaque analyse : dossier de variable ou ancêtre, `~`, `Documents` et ancêtres, corbeille, ce qui la contient, ce qu'elle contient, dossier de `autre` ou de root, FAT, FUSE | RGG-009 à RGG-012 | `settings_custom_folders` | oui |
| INV-608 | Dossier choisi accepté puis devenu dangereux avant l'analyse (remplacé par un lien vers `~`) : refusé à l'analyse, « Dossier à nettoyer refusé » | ajouter `~/vide`, puis `rm -r ~/vide; ln -s ~ ~/vide` → remarque, rien offert | `settings_custom_folders` | oui |

**Invariant 7 : un outil met à la corbeille, revérifie, garde une copie**

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| INV-701 | Doublons : la dernière copie d'un groupe ne part jamais, même toutes cochées | groupe de deux, cocher les deux (« Garder celles d'un dossier… » puis cocher à la main) → une reste | `inv_duplicates` | oui |
| INV-702 | La copie restante change après la recherche : rien ne part (`KeeperChanged`) | rechercher, ssh `echo >> gardienne` → copie demandée toujours là | `inv_duplicates` | oui |
| INV-703 | La copie demandée change, est remplacée ou retirée : elle reste | trois cas, chacun seul → « A changé depuis la recherche » | `inv_duplicates` | oui |
| INV-704 | Gardienne d'un autre compte : rien ne part | `autre` possède une copie dans un dossier partagé lisible → `NoSureKeeper` | `inv_duplicates` | oui (BIB-08) |
| INV-705 | Gardienne dans une corbeille : rien ne part | copie déjà dans `~/.local/share/Trash/files` ou un `$RECYCLE.BIN` d'une clé → aucune gardienne | `inv_duplicates` | oui |
| INV-706 | Gardienne sur FAT (`mtime` remis en arrière cache une écriture) ou alias FUSE (`bindfs` de la même copie) : rien ne part | BIB-11 → `NoSureKeeper` | `inv_duplicates` | oui |
| INV-707 | Un lien posé sur le chemin d'une copie avant le déplacement n'est jamais suivi | rechercher, remplacer le dossier parent par un lien → copie gardée | `inv_duplicates` | oui |
| INV-708 | Gros fichiers : fichier réécrit, remplacé, disparu, d'un nom de plus depuis la recherche → reste | quatre cas → raisons | `inv_large_files` | oui |
| INV-709 | Fichier d'un autre compte jamais listé (doublons et gros fichiers) | gros fichier de `autre` lisible dans `~/partage` → absent | `inv_large_files` | oui |
| INV-710 | Fichier sur un autre disque que la corbeille : case désactivée « Pas de corbeille sur ce disque » | gros fichier sur une boucle ext4 montée sans `.Trash-1000` possible (lecture seule) → désactivée | `inv_large_files` | oui |
| INV-711 | Reste d'appli : appli réinstallée entre analyse et mise à la corbeille → gardé | `~/snap/x` d'un snap absent, rechercher, `snap install --dangerous x` → gardé, « est de nouveau installé » | `inv_leftovers` | oui (BIB-10) |
| INV-712 | Reste d'appli : source illisible au moment du déplacement → gardé | rechercher, puis `systemctl stop snapd.socket snapd` → gardé | `inv_leftovers` | oui |
| INV-713 | Reste d'appli : empreinte changée (fichier écrit en profondeur, `mtime` remis en arrière), dossier remplacé, lien sur le parent → gardé | trois cas | `inv_leftovers` | oui |
| INV-714 | Reste d'appli : une exclusion (dessus, dessous, dedans), relue au déplacement | ajouter l'exclusion après la recherche → gardé | `inv_leftovers` | oui |
| INV-715 | Okapi confiné ne propose aucun reste | — | — | non : faire tourner Okapi en conteneur sort du banc ; `nothing_installed_is_known_when_okapi_runs_confined` |
| INV-716 | Une seule mise à la corbeille à la fois, tous outils confondus | lancer doublons et restes en même temps → le second attend (journal) | `inv_large_files` | partiel : course ; `a_move_waits_for_another_tool_s` |
| INV-717 | Un fichier mis à sa place par nom entre la vérification et le déplacement est remis en place | — | — | non : course ; `a_file_swapped_in_before_the_move_is_put_back` |
| INV-718 | Après déplacement, le fichier relu dans la corbeille est restaurable (`gio trash --list`, `.trashinfo` juste) | après un retrait de doublon → restaurer par `gio trash --restore` → contenu identique | `inv_duplicates` | oui |

### RGG : réglages

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| RGG-001 | « Chaque changement est enregistré tout de suite » : `settings.json` écrit à chaque changement, relu à l'ouverture de la page | changer l'âge → fichier changé ; modifier le fichier par ssh, rouvrir la page → valeur reprise | `settings_basics` | oui |
| RGG-002 | Âge minimal 30 min / 1 h / 2 h / 24 h, valeur hors liste dite « Réglé à {} min dans le fichier » ; effet sur l'analyse | fichier de 90 min ; 2 h → absent ; 1 h → présent | `settings_basics` | oui |
| RGG-003 | Journal des suppressions : chemins dans `history.jsonl` (`kind:"deletions"`) et « Détail par résultat » dans l'Historique | activer, nettoyer → ligne `deletions` ; désactiver → « Le détail par élément n'est gardé que si… » | `settings_basics` | oui |
| RGG-004 | Thème Système / Clair / Sombre appliqué tout de suite | capture avant et après | `settings_basics` | oui |
| RGG-005 | Langue : « S'applique au prochain démarrage » | LNG-002 | `lang_english` | oui |
| RGG-006 | Exclusions : ajout par champ et Entrée, par « Ajouter l'exclusion », par « Choisir un dossier à exclure » (portail), retrait « Retirer {} des exclusions », état vide | BIB-03 ; portail : choisir par AT-SPI dans le sélecteur GNOME | `settings_exclusions` | oui |
| RGG-007 | Refus d'exclusion : relative, `..`, déjà dans la liste, illisible, caractères impossibles | chaque saisie → message sous le champ ; une saisie efface le refus | `settings_exclusions` | oui |
| RGG-008 | Effet d'une exclusion `*.ext` (insensible à la casse) et d'un dossier exclu sur l'analyse, les doublons, les gros fichiers, les restes | `*.LOG` exclu → `x.log` gardé partout | `settings_exclusions` | oui |
| RGG-009 | Dossiers à nettoyer : ajout, badge, carte « Mes dossiers », le dossier lui-même jamais supprimé, un sous-dossier récent garde son contenu | ajouter `~/a-vider` avec fichiers vieillis → vidé, dossier là | `settings_custom_folders` | oui |
| RGG-010 | Refus : n'existe pas, lien, pas un dossier, `~`, `Documents`, ancêtre de `Documents`, dossier de variable (`~/.cache`), corbeille et ses dossiers, dossier de `autre`, de root | une saisie par cas → message attendu | `settings_custom_folders` | oui |
| RGG-011 | Refus sur FAT/exFAT et FUSE (« Dates peu sûres ») | BIB-11 | `settings_custom_folders` | oui |
| RGG-012 | Badges d'un dossier devenu absent, lien, illisible après ajout | changer par ssh, rouvrir → « Absent », « Lien », « Illisible » | `settings_custom_folders` | oui |
| RGG-013 | `settings.json` lien (dotfiles) : lecture seule, « Vos réglages sont un lien : modifiez-les à la main. », jamais écrit à travers | `ln -s ~/dotfiles/okapi.json` → contrôles désactivés, cible inchangée | `settings_files` | oui |
| RGG-014 | `settings.json` illisible : « {fichier} ne se lit pas… », « Relire », jamais remplacé ; lien mort, dossier, plus de 1 Mio, version inconnue | un cas par relance → bandeau, somme du fichier identique | `settings_files` | oui |
| RGG-015 | Modifié hors de l'écran depuis la lecture : « Vos réglages ont été modifiés hors de cet écran… », pas d'écriture | ouvrir la page, ssh change le fichier, changer un réglage → bandeau, fichier = version ssh | `settings_files` | oui |
| RGG-016 | Dossier de config non inscriptible : « Okapi ne peut pas enregistrer ses réglages » | `chmod 500 ~/.config/okapi` → bandeau | `settings_files` | oui |
| RGG-017 | « Journal technique » montre le dossier d'état | texte égal à `~/.local/state/okapi` | `settings_basics` | oui |
| RGG-018 | Le sélecteur de dossiers qui ne s'ouvre pas : « …écrivez le chemin. » | arrêter `xdg-desktop-portal` → message | `settings_exclusions` | oui |

### HIS : historique et reçus

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| HIS-001 | État vide « Aucune opération pour l'instant » | sans `history.jsonl` | `history_view` | oui |
| HIS-002 | Liste « Opérations, de la plus récente à la plus ancienne », en-têtes « Aujourd'hui », « Hier », date ; types « Nettoyage », « Nettoyage arrêté », « Désinstallation », « Ancienne version retirée », « Environnement retiré », « Mise à jour » ; icône « Administrateur » | `history.jsonl` fabriqué (une ligne de chaque `kind`, dates variées) → lignes | `history_view` | oui |
| HIS-003 | Clavier : focus à l'ouverture, flèches, Début, Fin choisissent une ligne | BIB-01 | `history_view` | oui |
| HIS-004 | Détail : « Espace libéré », « Retirées », « En partie », « Déjà parties », « Gardées », partie admin terminée ou non | ligne choisie → chiffres égaux au fichier | `history_view` | oui |
| HIS-005 | Lignes illisibles ignorées et comptées : « {n} ligne de l'historique, illisible ou incomplète, est ignorée. » | ajouter deux lignes cassées → pluriel | `history_view` | oui |
| HIS-006 | Historique illisible : « L'historique ne peut pas être lu. » | `history.jsonl` dossier → bandeau | `history_view` | oui |
| HIS-007 | Rotation à 8 Mio : `history.1.jsonl` avant un nettoyage | fichier de 8 Mio + 1 → nettoyer → `.1` créé, nouveau fichier d'une ligne | `history_view` | oui |
| HIS-008 | Rafraîchi à la fin d'un nettoyage si la page est affichée | lancer un nettoyage, aller sur Historique pendant → ligne apparaît | `history_view` | oui |
| HIS-009 | Journal des traces `okapi-app.log` supprimé au lancement s'il a plus de 14 jours, jamais à travers un lien | horloge +15 j → parti ; lien → gardé | `history_view` | oui |

### DBL : doublons

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| DBL-001 | « Où chercher ? » : dossiers personnels cochés, « Dossier personnel » coche et désactive les sous-dossiers | page Doublons → états | `duplicates_search` | oui |
| DBL-002 | Recherche : progression « Ouverture des dossiers… », « Liste des fichiers… », « Comparaison du contenu… », « Arrêter » → « Recherche arrêtée. Rien n'a été gardé » | 2 000 fichiers de 1 Mio → arrêter | `duplicates_search` | oui |
| DBL-003 | Ce qui est comparé : 1 Mio et plus, même contenu ; pas un fichier de même taille au contenu différent, ni un même début et une même fin | trios fabriqués → groupes justes | `duplicates_search` | oui |
| DBL-004 | Jamais parcourus : dossiers cachés, projets, `~/snap`, corbeille, liens, autres montages, exclusions | une copie dans chacun → absente | `duplicates_search` | oui |
| DBL-005 | « Aucun doublon dans ces dossiers », « Chercher dans d'autres dossiers » | dossiers sans doublon | `duplicates_search` | oui |
| DBL-006 | Arbre « Groupes de doublons, triés par place à gagner », « déplié / replié » | ordre par place | `duplicates_choose` | oui |
| DBL-007 | Cochage d'avance : la copie hors Téléchargements et Bureau est gardée, même plus récente ou d'autres noms ; une copie de chaque groupe toujours décochée | copie dans `Documents` (récente) et `Téléchargements` (ancienne) → `Documents` décochée | `duplicates_choose` | oui |
| DBL-008 | « Sélection rapide » : la plus ancienne, la plus récente, le chemin le plus court, celles d'un dossier ; « Tout désélectionner » | chaque choix → cases | `duplicates_choose` | oui |
| DBL-009 | Raisons de garde : « a d'autres noms », « pas de corbeille sur ce disque », « Seule copie gardée du groupe » | lien dur, boucle sans corbeille | `duplicates_choose` | oui |
| DBL-010 | Confirmation « Mettre à la corbeille ? », « Vers la corbeille : {}. Gardées : {}. », reçu, fichiers dans `~/.local/share/Trash` | confirmer → `gio trash --list` | `duplicates_trash` | oui |
| DBL-011 | « Montrer dans le dossier » ouvre Fichiers par le portail ; échec dit | nautilus présent → fenêtre ; portail arrêté → « Le gestionnaire de fichiers n'a pas pu montrer ce fichier » | `duplicates_trash` | oui |
| DBL-012 | Corbeille impossible à ouvrir : « La corbeille n'a pas pu être ouverte… », rien déplacé | `~/.local/share/Trash` fichier ordinaire | `duplicates_trash` | oui |
| DBL-013 | « Aucun dossier Documents, Images… connu » quand `user-dirs.dirs` n'en dit aucun | vider `~/.config/user-dirs.dirs` | `duplicates_search` | oui |

### GRF : gros fichiers

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| GRF-001 | Fichiers de 50 Mo et plus sur le disque, un fichier creux de 1 Go vide n'est pas « gros », un fichier à plusieurs noms listé une fois | `fallocate` 60 Mo, `truncate` 1 Go, lien dur → liste | `large_files_list` | oui |
| GRF-002 | Filtres « Taille minimale », « Type », ancienneté, « Effacer les filtres », états vides | chaque filtre → liste ; « Aucun fichier ne correspond à ces filtres » | `large_files_list` | oui |
| GRF-003 | Tri « Taille », « Date de modification » | ordre | `large_files_list` | oui |
| GRF-004 | Une case suit son fichier quand filtres ou tri changent | cocher, trier, filtrer → toujours le même fichier coché | `large_files_list` | oui |
| GRF-005 | Case désactivée : « A d'autres noms… », « Pas de corbeille sur ce disque » | lien dur, autre disque | `large_files_list` | oui |
| GRF-006 | « Mettre {n} fichier à la corbeille ? », reçu, fichier dans la corbeille | confirmer | `large_files_list` | oui |
| GRF-007 | Le fichier trouvé hors filtre est dit toujours là | « Le gros fichier trouvé est toujours là… » | `large_files_list` | oui |

### DSK : espace disque

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| DSK-001 | « Dossier personnel » ou « Ou un disque entier » ; « Analyser ce disque » ; « Lecture des disques… » | choix | `disk_usage_browse` | oui |
| DSK-002 | Tailles justes, liens durs comptés une fois (« Lien dur, compté dans {} »), creux dits, liens jamais suivis, autres montages « Monté à part, non parcouru ici » | arbre fabriqué → comparer à `du -x --apparent-size` | `disk_usage_browse` | oui |
| DSK-003 | Navigation : entrer, remonter, « Chemin complet », détail, « Fermer » | clics | `disk_usage_browse` | oui |
| DSK-004 | « Ouvrir dans le gestionnaire de fichiers » (portail) | fenêtre Fichiers | `disk_usage_browse` | oui |
| DSK-005 | Petits éléments regroupés, « Espace non expliqué » | 500 petits fichiers | `disk_usage_browse` | oui |
| DSK-006 | Illisible : « Illisible : il faut les droits administrateur » | disque entier `/` → `/root` illisible | `disk_usage_browse` | oui |
| DSK-007 | « Arrêter » → « Analyse arrêtée : tailles partielles » | gros arbre | `disk_usage_browse` | oui |
| DSK-008 | Disque débranché : « Ce disque n'est pas lu en ce moment… » | boucle montée, choisie, puis `umount -l` avant l'analyse | `disk_usage_browse` | oui |
| DSK-009 | Cet écran ne supprime rien ; « Aller au Nettoyeur » ouvre le Nettoyeur | sommes de contrôle de l'arbre avant et après | `disk_usage_browse` | oui |

### APP : applications installées et désinstallation

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| APP-001 | Liste : debs avec lanceur, snaps `app` avec lanceur, flatpaks système et utilisateur ; « Système et composants » replié, « C'est quoi ? » | `.deb` factice avec `.desktop` (BIB-10), snap local, flatpak → trois lignes | `apps_list` | partiel : un flatpak demande un runtime (réseau ou image de base, voir lots) |
| APP-002 | Filtre « Toutes / Ubuntu / Snap / Flatpak », recherche « Chercher une appli ou un paquet », « Effacer la recherche », « Aucune appli ne correspond » | BIB-03 | `apps_list` | oui |
| APP-003 | Tri « Nom », « Appli », « Données », « Place prise » | ordre | `apps_list` | oui |
| APP-004 | Détail : « Source », « Paquet », « Origine » (« Installée par vous », « avec une autre appli », « avec le système »), « Données », « Fermer le détail » | ligne choisie | `apps_list` | oui |
| APP-005 | États des sources : dpkg qui écrit (« une installation est en cours »), snapd qui ne répond pas, flatpak illisible ou en partie | `apt-get install` long en fond ; `systemctl stop snapd` ; `chmod 000` d'une installation flatpak → textes | `apps_list` | oui |
| APP-006 | Refus d'avance : paquet du bureau (« Le bureau a besoin de {}… »), flatpak de l'utilisateur (« Installée pour votre compte seulement… ») | gnome-terminal ; flatpak `--user` | `apps_list` | oui |
| APP-007 | Désinstaller un deb par PackageKit : simulation, « Désinstaller {} ? » avec ce qui part et la place, polkit, « {} désinstallée : {} libérés », ligne `uninstall` dans l'historique | `.deb` factice + un dépendant → les deux partis (`dpkg -s`) | `apps_uninstall_deb` | oui |
| APP-008 | Refus si la simulation emporte un paquet essentiel ou un métapaquet : « …retirerait aussi des paquets dont le système a besoin. Rien n'a été modifié. » | `.deb` factice dont dépend `ubuntu-desktop-minimal` (le métapaquet) → refus | `apps_uninstall_deb` | oui |
| APP-009 | Liste changée entre la vérification et le retrait : rien retiré, nouvelle liste montrée | ouvrir la confirmation, installer un nouveau dépendant par ssh, confirmer → rien parti | `apps_uninstall_deb` | oui |
| APP-010 | Autorisation refusée, service absent, autre installation en cours, « déjà désinstallée » | polkit NO ; `systemctl mask packagekit` ; `apt` qui tient le verrou ; `dpkg -r` par ssh avant confirmation | `apps_uninstall_deb` | oui |
| APP-011 | « Annuler la désinstallation », « Trop tard pour annuler » | annuler pendant le téléchargement factice (paquet local : très court) | `apps_uninstall_deb` | partiel : un retrait local finit trop vite pour annuler à coup sûr |
| APP-012 | Fermer Okapi pendant une désinstallation : deux fermetures | NAV-011 | `apps_uninstall_deb` | oui |
| APP-013 | Snap : « Supprimer aussi ses données » change `purge`, phrases des 31 jours ; reçu | snap local, avec et sans la case → `snap saved` montre une copie dans le premier cas seulement | `apps_uninstall_snap` | oui |
| APP-014 | Flatpak système : appli puis extensions, « Certaines de ses extensions sont restées installées » | flatpak avec extension | `apps_uninstall_flatpak` | partiel : runtime nécessaire |
| APP-015 | Appli ouverte : « {} est ouverte : Okapi va la fermer… », « Fermer et désinstaller », SIGTERM au scope, fermée → désinstallée | appli factice (`.desktop` qui lance un script Python à fenêtre) ouverte par `gtk-launch` → fermée, retirée | `apps_close_scopes` | oui |
| APP-016 | Elle ne se ferme pas : « Forcer la fermeture » → SIGKILL ; au second essai le bouton « Forcer » d'emblée ; échec « Elle n'a pas pu être fermée. » | script qui ignore SIGTERM | `apps_close_scopes` | oui |
| APP-017 | Chromium/Electron (second scope), flatpak (`app-flatpak-*`) : tous ses scopes fermés, aucun autre | deux scopes du même lanceur + un scope d'une autre appli → l'autre vit | `apps_close_scopes` | oui |
| APP-018 | « Actualiser », « Applis : {}, lues il y a {} min » | horloge +10 min | `apps_list` | oui |

### RST : restes d'applis et copies de snapd

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| RST-001 | Proposés : `~/snap/<nom>` sans snap, `~/.var/app/<id>` sans flatpak ; libellés « (snap désinstallé) », « (aucun snap de ce nom) » | dossiers fabriqués ; un snap installé puis retiré → libellés | `apps_leftovers` | oui |
| RST-002 | Écartés : montage, fichier d'un autre compte, illisible (« {} dossiers ne sont pas proposés par sécurité ») | trois cas | `apps_leftovers` | oui |
| RST-003 | « Trop de fichiers pour être vérifié » | reste de 200 000 fichiers | `apps_leftovers` | oui |
| RST-004 | Réglages système des debs désinstallés : lecture seule, « Gardés pour une réinstallation » | `.deb` factice avec conffile, `dpkg -r` (sans purge) | `apps_leftovers` | oui |
| RST-005 | « Mettre à la corbeille », confirmation, reçu, dossier dans la corbeille | confirmer | `apps_leftovers` | oui |
| RST-006 | Erreurs : réglages illisibles, dossiers XDG invérifiables, snaps ou flatpaks invérifiables | un cas par relance | `apps_leftovers` | oui |
| RST-007 | Copies de snapd : « Copie automatique de {}, faite {} », « Oublier », « Oublier la copie de {} ? », `forget`, « Copie de {} oubliée » | snap local retiré sans purge → copie ; oublier → `snap saved` vide | `apps_snap_copies` | oui |
| RST-008 | Snap réinstallé entre-temps : « {} est de nouveau installé : sa copie est gardée. » ; copie déjà effacée ; droits refusés | trois cas | `apps_snap_copies` | oui |

### OLD : anciennes versions

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| OLD-001 | Révisions gardées des snaps : « Révision {} », jamais celle en usage, ni une plus récente (revert), ni un snap désactivé, ni noyau ou gadget | snap local installé trois fois (`--dangerous`, x1 x2 x3), `snap revert` → x3 non proposé ; `snap disable` → rien | `apps_old_versions` | oui |
| OLD-002 | Rien coché d'avance, ni après un retrait ; une nouvelle recherche n'arrive jamais sous des cases cochées | après retrait → tout décoché | `apps_old_versions` | oui |
| OLD-003 | « Retirer un élément ? », « Vous ne pourrez plus revenir à ces versions… », « Retirer » | confirmer → `snap list --all` sans la révision ; ligne d'historique « Ancienne version retirée » | `apps_old_versions` | oui |
| OLD-004 | Élément changé avant le retrait : gardé, rien demandé | rechercher, `snap remove --revision` par ssh → gardé, journal sans requête | `apps_old_versions` | oui |
| OLD-005 | Environnements flatpak inutilisés : « Branche {} », « partagée », seulement si tout est lu ; ceux de l'utilisateur non proposés | runtime orphelin installé système et un `--user` | `apps_old_versions` | partiel : runtimes à fournir |
| OLD-006 | Place rendue : « Libéré tout de suite », « Rendu par snapd dans les 30 jours… » | textes | `apps_old_versions` | oui |
| OLD-007 | Droits refusés : « Rien n'a été retiré : les droits n'ont pas été donnés. » | polkit NO | `apps_old_versions` | oui |

### MAJ : mises à jour

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| MAJ-001 | Recherche à l'ouverture si la dernière date de plus de 5 min ; « Rechercher maintenant » rafraîchit le cache | dépôt apt local (BIB-10) avec une version plus récente du `.deb` factice → ligne | `updates_check` | oui |
| MAJ-002 | Liste « Paquets à mettre à jour, sécurité d'abord », « Sécurité », « Retenue », « Afficher les {} autres » | dépôt avec une suite `-security` et un paquet retenu (`apt-mark hold`) | `updates_check` | oui |
| MAJ-003 | Listes apt anciennes (plus de 7 jours) | horloge +8 j | `updates_check` | oui |
| MAJ-004 | Aucune source : « Okapi lit les mises à jour d'apt (PackageKit) et de snap. Aucun des deux… » | masquer packagekit et snapd | `updates_check` | oui |
| MAJ-005 | « Tout mettre à jour » : simulation, polkit, progression, « {} mises à jour installées », ligne `update` dans l'historique | dépôt signé (clé de confiance) → `dpkg -s` dit la nouvelle version | `updates_install` | oui |
| MAJ-006 | Paquets non signés refusés : « Des paquets ne sont pas signés par une source de confiance. » | dépôt `trusted=yes` sans signature | `updates_install` | oui |
| MAJ-007 | Mise à jour qui retire des paquets : modale « Cette mise à jour retire {} paquets », « Annuler » ne fait rien | nouvelle version en `Conflicts` avec un paquet installé | `updates_install` | oui |
| MAJ-008 | Rétrogradation refusée ; liste changée depuis la recherche (« …La liste vient d'être relue ») | changer le dépôt entre recherche et installation | `updates_install` | oui |
| MAJ-009 | Plus de place, dpkg interrompu (« sudo dpkg --configure -a »), autorisation refusée, « Annuler » | disque rempli ; dpkg tué au milieu ; polkit NO | `updates_install` | oui |
| MAJ-010 | Redémarrage : `/run/reboot-required` → « Redémarrage nécessaire » | `touch /run/reboot-required` | `updates_check` | oui |
| MAJ-011 | Snaps à mettre à jour, « Mettre à jour », « Mettre à jour toutes les applis », snap ouvert refusé | — | `updates_snaps` | non sans le store : `find?select=refresh` interroge le store ; faisable si la VM a le réseau (snap du store en vieille révision par `--revision`), sinon tests d'`okapi-tools` sur faux snapd |
| MAJ-012 | « Les applis snap se mettent à jour seules. Prochain passage {}. » | `/v2/system-info` local | `updates_check` | oui |
| MAJ-013 | Retrait jamais montré, détecté et dit « Le système a aussi retiré : {} » | — | — | non : il faudrait un PackageKit qui ment ; tests de `packages/` sur faux bus |

### DCK : Docker

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| DCK-001 | Ligne « Cache de construction Docker » jamais cochée, « Docker élaguera son cache… » | `tester` dans le groupe docker (BIB-09) | `docker_prune` | oui |
| DCK-002 | L'analyse ne démarre pas le démon (socket activée) | `systemctl stop docker.service`, `docker.socket` actif → analyse → `docker.service` toujours inactif | `docker_prune` | oui |
| DCK-003 | Cochée : `POST /build/prune` avec `until` à 168 h (ou l'âge des réglages s'il est plus long), `keep-storage` 10 Go ; reçu « Docker : {n} cache retiré » | `docker build` hors réseau (`FROM scratch`, `COPY`), horloge +8 j → cache vidé (`docker system df`) ; requête lue par `socat` interposé ou journal du démon | `docker_prune` | partiel : sous 10 Go, `keep-storage` garde tout ; régler l'assertion sur la requête plutôt que sur le cache |
| DCK-004 | Compte refusé : « Docker refuse votre compte… », ligne non cochable, Docker jamais sollicité | `tester` hors du groupe → note ; aucune connexion (journal du démon) | `docker_prune` | oui |
| DCK-005 | Moteur arrêté, refus, injoignable : lignes du reçu | arrêter docker entre analyse et nettoyage → « Docker : rien de retiré, le moteur ne tourne pas. » | `docker_prune` | oui |
| DCK-006 | Nettoyage annulé avant l'étape Docker : « Docker : pas demandé… » | annuler pendant la suppression | `docker_prune` | oui |
| DCK-007 | Socket choisie : `DOCKER_HOST`, puis `/var/run/docker.sock`, `$XDG_RUNTIME_DIR/docker.sock` (rootless) | lancer avec `DOCKER_HOST` vers une socket rootless | `docker_prune` | partiel : rootless à provisionner (`docker-ce-rootless-extras` absent d'Ubuntu) |

### CLI : ligne de commande `okapi`

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| CLI-001 | `okapi` seul : analyse, une ligne par groupe avec sa décision, « Journal : chemin », exit 0 ; rien supprimé | fichiers vieillis → sortie, fichiers là | `cli_scan` | oui (BIB-05) |
| CLI-002 | `--clean` : seuls les groupes `selected` partent ; `needs_admin`, `not_preselected` (cacheReset, maintenance, corbeille, custom) restent ; le helper n'est jamais lancé | un fichier de chaque sorte → seuls les présélectionnés partis ; `journal.txt` sans pkexec | `cli_clean` | oui |
| CLI-003 | Catégories `--system`, `--browsers`, `--apps`, `--gaming`, `--gpu`, cumulables ; sans aucune, toutes sauf custom | sorties | `cli_scan` | oui |
| CLI-004 | `--custom` seul parcourt les dossiers choisis, et `--clean --custom` ne les nettoie jamais | dossier choisi vieilli → listé, toujours là après `--clean` | `cli_clean` | oui |
| CLI-005 | `--exclude` (répétable, `*.ext`), exclusion absente : avertissement ; invalide : exit 2 | trois cas | `cli_options` | oui |
| CLI-006 | `--min-age` : 0 accepté, `-1`, `ten`, `1.5`, `18446744073709551615` refusés exit 2 ; l'emporte sur les réglages | cas | `cli_options` | oui |
| CLI-007 | `--json` : un document, champs `groups`, `decision`, `clean.outcomes` | `python3 -m json.tool` | `cli_scan` | oui |
| CLI-008 | `-v` liste les entrées et copie le journal sur stderr ; `-q` le résultat seul ; `-v -q` exit 2 | cas | `cli_options` | oui |
| CLI-009 | Codes 3 (permission refusée), 4 (partiel : récent, exclu...), 5 (rien trouvé) | dossier vieilli rendu non inscriptible (`chmod 555`) dans `~/.cache/thumbnails` → 3 ; fichier `chattr +i` par root (échec autre qu'un refus de droits) → 4 ; catégorie vide → 5 | `cli_exit_codes` | partiel : un fichier rajeuni entre analyse et nettoyage ne se monte pas en un seul processus ; le code 4 passe par un `Failed` |
| CLI-010 | Ctrl+C : 1er annule proprement (130), 2e pendant l'analyse sort aussitôt ; pendant `--clean`, la ligne d'historique est écrite avant de sortir | `timeout -s INT` sur un gros nettoyage → ligne `history.jsonl` présente, exit 130 | `cli_exit_codes` | oui |
| CLI-011 | Lancé en root : refus, exit 1 | ssh root | `cli_exit_codes` | oui |
| CLI-012 | Réglages illisibles (exit 1), invalides (exit 2), `customFolders` relatif (exit 2) ; les défauts ne les remplacent jamais | cas | `cli_options` | oui |
| CLI-013 | Historique indisponible avec `deletionLog` : exit 1, rien retiré | `history.jsonl` dossier | `cli_clean` | oui |
| CLI-014 | `okapi licenses`, `--help` (chemins réels des fichiers), `--version`, arguments inconnus (`--`, `licenses --clean`) exit 2, `--help` gagne après une erreur | cas | `cli_options` | oui |
| CLI-015 | Pipe fermé (`\| head -1`) silencieux, exit 0 | cas | `cli_options` | oui |
| CLI-016 | Langue du CLI : `LANG=en_US.UTF-8` → messages anglais | `LANG=` | `cli_options` | oui |
| CLI-017 | Les invariants valent aussi par le CLI : lien planté, fichier rajeuni, `TMPDIR` sous `/tmp/user` | refaire INV-301, INV-601 en `okapi --clean` | `cli_clean` | oui |

### LNG : langue anglaise

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| LNG-001 | Session en anglais (`LANG=en_US.UTF-8`) : toute l'interface et les libellés des règles en anglais | BIB-04, noms anglais dans le pilote → « Cleaner », « Scan », « Results » | `lang_english` | oui |
| LNG-002 | Réglage « English » : appliqué au prochain démarrage, pas avant | changer, vérifier que l'écran reste français, relancer → anglais | `lang_english` | oui |
| LNG-003 | Priorités : `LC_ALL=C` → anglais, `LANGUAGE=en:fr` avec `LANG=fr_FR` → anglais, `de_DE` → anglais | trois lancements | `lang_english` | oui |
| LNG-004 | Parcours complet du nettoyeur en anglais (analyse, confirmation « I understand… », reçu) | refaire `cleaner_admin` en anglais | `lang_english` | oui |
| LNG-005 | Aucun texte français restant ni tiret cadratin dans l'arbre d'accessibilité anglais, chaque page | parcourir les pages, `tree` → aucune chaîne du catalogue français non traduite, aucun « — » | `lang_english` | oui |
| LNG-006 | Pluriels anglais (« 1 file », « 2 files ») | reçu à 1 et 2 éléments | `lang_english` | oui |

### CLV : clavier et accessibilité

| Id | Vérifie | Montage en VM | Scénario | VM |
|---|---|---|---|---|
| CLV-001 | Échap ferme chaque modale sans agir : « Supprimer définitivement ? », question navigateur, « Ajouter un widget », « Rétablir… », « Désinstaller {} ? », « Mettre à la corbeille ? », « Retirer… », « Oublier la copie… », « Cette mise à jour retire… » | ouvrir chacune, BIB-01 Échap → fermée, rien fait | `keyboard_modals` | oui |
| CLV-002 | Un clic hors d'une modale ne la ferme jamais | BIB-02 clic dans un coin de la fenêtre → modale toujours là | `keyboard_modals` | oui |
| CLV-003 | Focus initial : « Annuler » dans la confirmation, « Ignorer {nav} » dans la question navigateur, « Fermer » dans « Ajouter un widget » | état `focused` | `keyboard_modals` | oui |
| CLV-004 | Parcours tout clavier du nettoyeur : Tab jusqu'à « Analyser », Espace sur les cases, Entrée sur les boutons, jusqu'au reçu | BIB-01 seul, aucun clic | `keyboard_tour` | oui |
| CLV-005 | Un bouton désactivé est sauté par Tab | « Supprimer définitivement » sans la case → Tab ne s'y arrête pas | `keyboard_tour` | oui |
| CLV-006 | Une touche maintenue n'active pas en boucle (bouton, titre de groupe) | QMP touche tenue 2 s → un seul basculement | `keyboard_tour` | oui |
| CLV-007 | Listes défilantes : flèches, Page préc./suiv., Début, Fin | Historique, Licences | `keyboard_tour` | oui |
| CLV-008 | Bandeaux lus à leur apparition (`live-region`) | Orca actif, journal d'Orca (`orca --debug`) → bandeau annoncé | `keyboard_orca` | partiel : lecture d'Orca à confirmer comme preuve fiable |
| CLV-009 | Noms et descriptions accessibles : chaque bouton à icône a un nom, chaque case son état, l'en-tête trié dit son sens | parcourir l'arbre de chaque page → aucun nœud interactif sans nom | `keyboard_tour` | oui |

## Comptes par domaine

| Domaine | Entrées | dont `non` |
|---|---|---|
| NAV | 13 | 0 |
| ACC | 19 | 0 |
| CLN | 39 | 0 |
| ADM | 23 | 1 |
| RGL | 24 | 2 |
| INV | 67 | 7 |
| RGG | 18 | 0 |
| HIS | 9 | 0 |
| DBL | 13 | 0 |
| GRF | 7 | 0 |
| DSK | 9 | 0 |
| APP | 18 | 0 |
| RST | 8 | 0 |
| OLD | 7 | 0 |
| MAJ | 13 | 2 |
| DCK | 7 | 0 |
| CLI | 17 | 0 |
| LNG | 6 | 0 |
| CLV | 9 | 0 |
| **Total** | **326** | **12** |

INV se répartit en 8 (invariant 1), 9 (2), 10 (3), 8 (4), 6 (5), 8 (6), 18 (7).

## Lots

Chaque lot est un ensemble de scénarios qu'une session écrit seule, en une journée au plus. Aucun fichier n'est partagé entre lots hors la bibliothèque commune (`okapivm/`, `guest/`) et `guest/provision.sh`. Un lot qui ajoute un paquet à l'image de base le fait dans un commit à part, qui ne touche que `provision.sh` : l'image de base se refait alors (`okapi-vm prepare`), ce que la note du lot dit. Un lot qui a besoin d'un élément BIB absent l'ajoute à la bibliothèque, en générique, dans un commit à part, et le signale aux sessions des autres lots.

| Lot | Domaine | Scénarios | Entrées | Dépendances |
|---|---|---|---|---|
| L01 `nettoyeur` | CLN | 8 : `cleaner_categories`, `cleaner_scan`, `cleaner_results`, `cleaner_kept_choices`, `cleaner_confirm`, `cleaner_clean`, `cleaner_problems`, `cleaner_browsers` | CLN-001 à 039 | BIB-01, 02, 04, 06, 12, 13 |
| L02 `admin` | ADM | 5 : `admin_rights`, `admin_helper_install`, `admin_helper_protocol`, `admin_journal`, `admin_setting_off` (et `cleaner_admin`, existant) | ADM-001 à 022, INV-104, 105, 307 | BIB-05, 07 ; un helper d'une version antérieure du protocole (ADM-008) construit d'un vieux commit ; BIB-10 pour ADM-022 |
| L03 `regles` | RGL | 6 : `rules_system`, `rules_system_admin`, `rules_trash`, `rules_browsers`, `rules_apps`, `rules_gaming_gpu` | RGL-001 à 022, INV-208, 505, 506 | BIB-06, 13 ; `wl-clipboard` dans l'image (RGL-018) |
| L04 `invariants-chemins` | INV 1, 3, 6 | 4 : `inv_root_owners`, `inv_links`, `inv_variables`, `inv_no_command` | INV-101 à 103, 301 à 309, 401 à 403, 601 à 605 | BIB-04, 08, 09, 11 ; `libpam-tmpdir` et `auditd` dans l'image (ou installés par le scénario), `bindfs` |
| L05 `invariants-temps` | INV 2, 5 | 2 : `inv_age`, `inv_preselection` | INV-201 à 207, 501 à 504 | BIB-06 ; utilise L02 pour INV-207 (archives du journal) : l'entrée va dans `admin_journal`, écrite par L02 |
| L06 `corbeille-fichiers` | DBL, GRF, INV 7 (doublons, gros fichiers) | 6 : `duplicates_search`, `duplicates_choose`, `duplicates_trash`, `large_files_list`, `inv_duplicates`, `inv_large_files` | DBL, GRF, INV-701 à 710, 716, 718 | BIB-08, 11 ; `dosfstools` et `bindfs` dans l'image |
| L07 `applis` | APP, INV 4 (scopes) | 5 : `apps_list`, `apps_uninstall_deb`, `apps_uninstall_snap`, `apps_uninstall_flatpak`, `apps_close_scopes` | APP-001 à 018, NAV-011, INV-405, 406 | BIB-03, 07, 10 ; un runtime et une appli flatpak dans l'image de base (Flathub, réseau à la préparation, environ 1 Go) ; `dbus-monitor` (présent) |
| L08 `restes-et-versions` | RST, OLD, INV 7 (restes), INV 4 (snapd, flatpak) | 4 : `apps_leftovers`, `apps_snap_copies`, `apps_old_versions`, `inv_leftovers` | RST, OLD, INV-408, 711 à 714 | BIB-07, 10 ; même runtime flatpak que L07 (plus un second runtime orphelin) ; partage la fabrique de snap local avec L07 : dans la bibliothèque |
| L09 `mises-a-jour` | MAJ | 3 : `updates_check`, `updates_install`, `updates_snaps` | MAJ-001 à 012, ACC-018 | BIB-07, 10 (dépôt apt local signé) ; `dpkg-dev` et `gnupg` dans l'image ; MAJ-011 seulement si la VM a le réseau au moment du scénario |
| L10 `accueil-reglages-historique` | ACC, RGG, HIS | 8 : `home_layout`, `home_recoverable`, `home_monitor`, `settings_basics`, `settings_exclusions`, `settings_custom_folders`, `settings_files`, `history_view` | ACC (sauf 018), RGG, HIS, INV-607, 608 | BIB-01, 03, 06, 11 ; `nfs-kernel-server` pour ACC-014 (facultatif) |
| L11 `cli-docker-disque` | CLI, DCK, DSK | 6 : `cli_scan`, `cli_clean`, `cli_options`, `cli_exit_codes`, `docker_prune`, `disk_usage_browse` | CLI, DCK, DSK, INV-107, 404 | BIB-05, 09 (groupe docker) ; Docker est déjà dans l'image |
| L12 `navigation-langue-clavier` | NAV, LNG, CLV | 6 : `nav_sidebar`, `nav_closing`, `lang_english`, `keyboard_modals`, `keyboard_tour`, `keyboard_orca` | NAV (sauf 011), LNG, CLV | BIB-01, 02, 04 ; le pilote doit accepter les noms anglais (il cherche par nom affiché : rien à changer) ; CLV-001 passe par les modales des autres écrans, ouvertes par ses propres préparations |

Ordre conseillé : la bibliothèque (BIB-01 à 06) d'abord, car presque tous les lots en ont besoin ; puis L01 et L02, qui fixent la manière d'écrire un scénario du nettoyeur ; les autres en parallèle. L07 et L08 attendent la même image de base (flatpak) : la préparer une seule fois avant de les lancer. Chaque scénario se lance seul (`okapi-vm run <nom>`) ; les entrées d'un même scénario se suivent sur la même VM, donc une entrée qui casse l'état d'une autre (`chmod 000`, démon arrêté) passe en dernier ou rétablit l'état.
