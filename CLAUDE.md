# Okapi

Nettoyeur et outil d'entretien système de bureau, natif en Rust (Linux d'abord, puis Windows et macOS), inspiré de Kudu (Electron, licence MIT).

Ce fichier ne porte que ce qui change le code qu'on écrit. Le « pourquoi » vit dans `docs/regles/`, les décisions dans `docs/TODO.md` § Décisions actées. Il est chargé à chaque session et par chaque sous-agent : chaque ligne se paie, garde-le court.

## Stack

- Rust 1.99, édition 2024, workspace Cargo. Interface en Slint. Textes affichés en français (source) et anglais.
- Conventions et pièges de la stack : skill `socle-rust:rust`. Ce fichier l'emporte en cas de conflit.

## Commandes

- Build : `cargo build --workspace`
- Tests : `cargo test --workspace`
- Lint : `cargo clippy --workspace --all-targets -- -D warnings` et `cargo fmt --all --check`
- Lancer en local : `cargo run -p okapi-app` (interface), `cargo run -p okapi -- <args>` (CLI)

## Architecture

- `crates/rules` (`okapi-rules`) : types serde des règles de nettoyage, résolution des variables (`${CACHE}`...), validation, textes affichés des règles et langue de la session (`texts`). Pas d'E/S hors lecture.
- `crates/platform` (`okapi-platform`) : traits par domaine (chemins, élévation, démarrage, services, réseau...), un module par OS sous `#[cfg(target_os)]`.
- `crates/engine` (`okapi-engine`) : scan, cache des résultats par ID, nettoyage sûr, reçus. Progression par channel, annulation par jeton.
- `crates/store` (`okapi-store`) : réglages, historique (journal des suppressions compris), journal des traces de chaque binaire (`journal`, crochet de panique).
- `crates/tools` (`okapi-tools`) : un module par outil (doublons, gros fichiers, disque, malware...), activable par feature Cargo.
- `crates/helper` (`okapi-helper`) : binaire privilégié lancé via pkexec (`packaging/linux/`), liste fermée d'opérations ; sa bibliothèque porte le protocole. Il n'écrit aucun fichier à lui (pas de journal) : ses traces partent en JSON sur sa sortie d'erreur, que l'interface (`crates/app/src/elevation.rs`, le seul lanceur) lit au fil de l'eau et verse à son journal.
- `crates/cli` (`okapi`) et `crates/app` (`okapi-app`, Slint) : coquilles fines au-dessus d'`engine` et `tools`.
- `engine`, `tools`, `store` et `rules` ne dépendent jamais de Slint ni de `app`.

## Conventions

- Exigence : du Rust idiomatique et irréprochable. Ce qui suit n'est pas négociable, une revue le fait respecter.
- Code et identifiants en anglais, sans abréviation (sauf acronymes usuels et `i`, `j`, `k`).
- Aucune chaîne affichée en dur, aucun nombre magique : traductions (`@tr()` côté Slint) et constantes nommées.
- KISS : la solution la plus simple qui marche, pas d'abstraction spéculative, imiter l'existant plutôt que créer un mécanisme parallèle.
- DRY : chaque connaissance (règle métier, constante, chemin, format) a une seule source. Deux codes qui se ressemblent par hasard ne sont pas une duplication.
- SOLID : une responsabilité par module et par type ; `engine` et `tools` dépendent des traits d'`okapi-platform`, jamais d'une implémentation d'OS, ce qui les rend testables avec des doublures.
- Erreurs typées (`thiserror`) dans les bibliothèques, `anyhow` seulement dans les binaires. Pas d'`unwrap`/`expect` hors tests, pas d'erreur avalée.
- Observabilité soignée : Franck teste, Claude étudie les traces. `tracing` dans les bibliothèques (une portée par opération, des champs structurés : chemin, raison, code d'erreur système, comptes, durées), jamais une erreur ou un refus sans trace ; les binaires écrivent le journal dans un fichier sous `~/.local/state/okapi/` (`tracing-subscriber`).
- Les lints de `[workspace.lints]` (clippy pedantic, `unsafe_code` interdit...) sont la norme : on ne les affaiblit jamais globalement. Une exception est locale et justifiée : `#[expect(lint, reason = "...")]`.
- Toute bibliothèque a sa doc de module (`//!`) et ses tests unitaires à côté du code.
- Un commentaire n'existe que pour faire comprendre le code, expliquer une correction contre-intuitive, ou retenir une erreur qu'on referait. Jamais de récit, jamais de compte recopié.
- Aucun tiret cadratin dans un texte affiché.
- Une modale ne se ferme jamais par un clic en dehors : seulement par ses boutons, Échap ou retour.

## Invariants stricts

Numérotés, jamais renumérotés (des agents et des tests les citent par numéro). Un invariant retiré garde son numéro, marqué MORT.

1. L'interface et le CLI ne transmettent jamais un chemin à supprimer : seulement des identifiants issus du dernier scan, résolus par `engine`. `okapi-helper` ne reçoit que les chemins modèles, sans variable, de cibles `needsAdmin` qui suppriment, dans ses propres règles (aucune autre cible qui supprime n'a le même chemin, `okapi-rules`), refait l'analyse en root et ne nettoie que les groupes dont toutes les règles sont demandées. (`okapi-engine` : rapport scellé par visibilité, identifiant d'un autre rapport refusé à la résolution (`ScanReport::find`) comme au nettoyage, testé ; interface : identifiants des groupes cochés du rapport gardé, nettoyage de bout en bout testé en bac à sable ; sa part de root ne nomme que les chemins modèles des groupes administrateur cochés, jamais ses identifiants au moteur de l'utilisateur, testé ; `okapi-helper` : demande refusée en entier au moindre écart, groupe partagé avec une règle non demandée gardé, testé)
2. L'âge minimal d'un élément est revérifié juste avant sa suppression, descendants compris. (`okapi-engine`, `clean` : passe de vérification puis relecture de chaque entrée au moment de la retirer, testé)
3. Le scan et la suppression ne suivent jamais un lien symbolique ; les exclusions se comparent après résolution du chemin réel. (`okapi-engine` : scan par chemin, chaque dossier ouvert revérifié ; nettoyage par descripteurs ; testé)
4. Une règle est de la donnée : elle ne fournit jamais de commande, d'exécutable ni d'argument. `okapi-helper` n'exécute que des opérations figées dans son code ; l'interface ne lance qu'une commande, pkexec sur le helper, programme et arguments fixes (`crates/app/src/elevation.rs`). (`clippy.toml` : `std::process::Command` et l'envoi de signaux de std, rustix et `okapi-platform` interdits partout, exceptions locales par `#[expect]`, et `libc` n'est appelable qu'en `unsafe`, interdit ; `okapi-helper` : liste de ses dépendances figée d'après `Cargo.lock`, testé)
5. Les réinitialisations de cache de performance et les maintenances natives sont décochées par défaut. (`okapi-rules` : `is_selected_by_default`, testé ; `okapi-engine` : ET des règles qui partagent une racine, même pour une analyse limitée à des catégories, testé ; interface : catégorie « À choisir » décochée, groupes cochés par `checked_by_default` du moteur, case de catégorie ou de navigateur qui ne coche jamais un « À choisir », testé)
6. Une racine de suppression résolue et canonisée n'est jamais égale à la valeur d'une variable (`HOME`, `CONFIG`, `SYSTEMDRIVE`...) ni un de ses ancêtres, `TMPDIR` excepté, que l'on ne vide qu'une fois vérifié qu'il ne contient aucune autre variable. `okapi-rules` n'en fait qu'un contrôle lexical sur les règles ; le moteur le refait par identité de fichier (périphérique et inode) des valeurs et de leurs ancêtres. (`okapi-engine`, scan et nettoyage : testé ; le passage à travers un alias ou un montage bind n'est simulé qu'au scan)

## Git

- Conventional Commits `type(scope): description`, en français, un commit par sujet.
- Jamais d'attribution à l'IA (ni Co-Authored-By, ni « Generated with »).
- Commit et push via la skill `socle:push-own`. Jamais `stash`, `reset`, `restore`, `checkout -- <fichier>`, `clean`, `add -A`, `commit -a`, `commit --amend` : d'autres sessions travaillent peut-être sur la même copie.
- Une entrée de `docs/TODO.md` livrée est supprimée dans le commit qui la livre.

## Flux

- Avant la spec ou le plan de tout chantier : regarder ce qui existe en ligne (outils open source qui font la même chose, Kudu, leurs retours d'utilisateurs) et en tirer le meilleur. Le plan dit ce qui a été regardé, ce qu'on reprend et pourquoi.
- Plus de 4 fichiers ou un sujet délicat : skill `socle:livrer` (plan, revue du plan, code, revue).
- Clôture d'une feature : skill `socle:review-and-fix`.
- Une correction de Franck ou un piège découvert : skill `socle:lessons`.

## Carte des docs

| Quand | Lire |
|---|---|
| avant de toucher un invariant | `docs/regles/INVARIANTS.md` |
| avant de toucher le scan | `docs/designs/TODO-3.md` |
| avant de toucher le nettoyage | `docs/designs/TODO-4.md` |
| avant de rouvrir un choix | `docs/TODO.md` § Décisions actées |
| avant de toucher l'interface | `docs/design/maquette/project/` (jetons : `okapi.css`, à refléter dans `crates/app/ui/theme.slint`), puis `docs/design/brief-claude-design.md` |
