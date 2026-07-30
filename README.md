# gitwatch

Surveille un dossier de dépôts git (ex. `~/dev`) et les synchronise. Pensé pour
NixOS + Hyprland + Waybar, mais utilisable partout où `git` est disponible.

Trois sous-commandes :

| Commande | Rôle |
|----------|------|
| `gitwatch waybar_status <dir>` | Fetch **tous** les dépôts en parallèle et imprime une ligne JSON pour un module custom Waybar (`text`, `tooltip`, `class`). |
| `gitwatch rofi_list <dir>` | Imprime `<icône> <nom>` par dépôt (les *dirty* d'abord). Sans fetch par défaut (rapide) — pour un menu rofi. |
| `gitwatch sync <repo>` | Commit tout, fetch, rebase sur `origin/<branche>`, push. En cas de conflit : `rebase --abort`, range le travail local sur une branche `sync/conflit-*`, la pousse, et réaligne `<branche>` sur l'upstream. |

## Choix techniques

- **Appel du binaire `git`** en sous-processus : `fetch`/`push` réutilisent
  automatiquement ta config credentials (agent SSH, credential helpers), sans
  aucune configuration côté gitwatch.
- **Parallélisme par threads OS bornés** (pas de rayon ni tokio) : le vrai I/O
  réseau vit *dans* le process `git` enfant, donc un thread bloqué en attente ne
  coûte quasiment rien. `-j N` plafonne le nombre de `git` lancés simultanément
  (défaut 16).

## Build

### Avec Nix (recommandé sur NixOS)

Le flake enveloppe le binaire pour que `git` soit toujours dans son `PATH`.

```bash
nix build            # -> ./result/bin/gitwatch
nix run . -- waybar_status ~/dev
```

Dans une config flake, ajoute l'entrée puis :

```nix
environment.systemPackages = [ inputs.gitwatch.packages.${pkgs.system}.default ];
# ou home.packages = [ ... ] côté home-manager
```

### Avec Cargo

```bash
cargo build --release      # -> ./target/release/gitwatch
```

> ⚠️ Le binaire pré-compilé fourni dans le zip est lié à la glibc via
> `/lib64/ld-linux-x86-64.so.2` et **ne tournera pas tel quel sur NixOS**
> (chemin FHS absent). Sur NixOS, construis via `nix build` ou `cargo`, ou
> utilise `nix-ld`/`patchelf` sur le binaire.

## Format de sortie `waybar_status`

```json
{
  "text": " 12 ↑3 ↓5 ⇅1 ●2 ⇡1 ⚠1",
  "tooltip": "…/dev — 12 repos\n↑ api      main  ↑3\n● dotfiles main  ●2\n…",
  "class": "conflict"
}
```

Résumé compact (groupes non nuls seulement) :

| Glyphe | Signification |
|--------|---------------|
|  N  | nombre total de dépôts |
| ↑ | dépôts en avance (commits à push) |
| ↓ | dépôts en retard (commits à pull) |
| ⇅ | dépôts divergents (avance **et** retard) |
| ● | dépôts avec modifications locales |
| ⇡ | branche jamais poussée (pas d'upstream) |
| ⚠ | conflits présents dans l'arbre |
| ? | remote injoignable / erreur |

`class` prend la valeur la plus prioritaire :
`error > conflict > unpublished > diverged > behind > ahead > dirty > clean`
— pratique pour colorer le module en CSS (voir `examples/waybar-style.css`).

## Codes de sortie de `sync`

Identiques au script bash d'origine :

- `0` — synchronisé (ou premier push effectué)
- `2` — **conflit** : le travail local est sur une branche `sync/conflit-<branche>-<epoch>`
  (poussée sur `origin`), `HEAD` est dessus, et `<branche>` a été réalignée sur l'upstream
- `1` — erreur (detached HEAD, pas un dépôt, run concurrent, push refusé, …)

Un verrou `flock` non bloquant sur `<git-dir>/auto-sync.lock` empêche deux `sync`
simultanés sur le même dépôt.

## Options

```
-j N        nb max de process git concurrents (scans)   [défaut 16]
--no-fetch  ne pas fetch (waybar_status)
--fetch     forcer le fetch (rofi_list)
-h, --help  aide
--version   version
```

## Intégration

Voir le dossier [`examples/`](examples) :

- `waybar-config.jsonc` — module custom (`interval` + `signal` 8 pour rafraîchir)
- `waybar-style.css` — couleurs par `class`
- `rofi-gitmenu.sh` — menu rofi qui appelle `rofi_list` puis `sync`
- `hyprland.conf` — bind `SUPER+G` + exemple de sync périodique

Copie le script rofi dans `~/.config/waybar/scripts/` et rends-le exécutable.
Le dossier des dépôts se surcharge via la variable `GITWATCH_DIR`.
