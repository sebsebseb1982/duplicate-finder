# Duplicate Finder

Application graphique (Windows / Linux, Rust + egui) pour trouver et supprimer
les fichiers **binairement identiques** dans un ou plusieurs dossiers.

## Compilation

```sh
cargo build --release
# binaire : target/release/duplicate-finder(.exe)
```

Sous Windows, la toolchain MSVC de Rust suffit (`rustup default stable-msvc`).
Sous Linux, aucune bibliothèque de développement n'est requise : OpenGL, X11 et
Wayland sont chargés dynamiquement à l'exécution.

Des dossiers peuvent être passés en argument : `duplicate-finder ~/Photos /mnt/sauvegarde`.

## Fonctionnement

### Recherche

1. Le parcours des dossiers et le hachage tournent en parallèle : les résultats
   apparaissent **pendant** l'analyse et peuvent être traités immédiatement.
2. Un fichier n'est lu que si un autre fichier de même taille existe.
3. Empreinte partielle (64 Kio de début + 64 Kio de fin), puis empreinte
   complète BLAKE3 seulement si les empreintes partielles coïncident.
4. Les liens symboliques sont ignorés ; sous Linux, les liens physiques et
   montages liés (même inode) ne sont comptés qu'une fois. Un dossier inclus dans
   un autre dossier analysé n'est pas parcouru deux fois.

Sur disque dur mécanique, réglez « Threads de lecture » sur 1 ou 2.

Les fichiers sont toujours comparés **en entier**, octet par octet : l'intérieur
des fichiers n'est jamais analysé. Deux morceaux (mp3, flac…) partageant la même
pochette mais pas le même son ne sont pas des doublons, et un `cover.jpg` n'est
jamais comparé à la pochette intégrée d'un mp3. Les jpg signalés en double dans
une discothèque sont de vrais fichiers séparés, souvent cachés (`Folder.jpg`,
`AlbumArt_{…}_Large.jpg`, `AlbumArtSmall.jpg` créés par Windows Media Player) :
excluez-les avec un motif, par exemple `*/AlbumArt*.jpg`.

### Exclusions

Motifs glob (même syntaxe que les règles) écartés de la recherche :

| Motif | Effet |
|---|---|
| `*/.git` | ignore les dossiers `.git` et tout leur contenu (non parcourus) |
| `*/AlbumArt*.jpg` | ignore ces fichiers |
| `*.{tmp,part}` | ignore ces extensions |

Une exclusion s'applique immédiatement : aux résultats déjà affichés (masqués,
pas supprimés), à l'analyse en cours et aux suivantes. Retirer une exclusion
réaffiche les résultats masqués ; les fichiers qui n'ont pas été parcourus à
cause d'elle ne reviennent qu'en relançant l'analyse.

Pendant la saisie d'un motif (exclusion ou règle), le nombre de doublons et de
dossiers concernés parmi les résultats actuels s'affiche en direct ; pour une
règle, avec le nombre de fichiers qui seraient réellement marqués.

### Résultats

- **Arborescence** : dossiers contenant des doublons (les chaînes de dossiers
  uniques sont fusionnées). La case d'un dossier marque / démarque tout son
  contenu ; clic droit pour ouvrir ou démarquer.
- **Groupes** : liste des groupes triés par espace récupérable.
- **Aperçu** (panneau de droite) : image, texte ou premiers octets en hexadécimal,
  ainsi que la liste des exemplaires identiques avec « Garder » (conserve celui-ci,
  marque les autres).

Couleurs :

| Couleur | Signification |
|---|---|
| Gris / couleur du texte | à traiter |
| Rouge | marqué pour suppression |
| Vert | exemplaire conservé : tous ses doublons sont marqués |
| Orange | tous les exemplaires d'un groupe sont marqués : **rien ne sera supprimé** pour ce groupe |

Un dossier est rouge si tous ses doublons sont marqués, vert si tous sont
conservés, orange s'il contient un conflit.

### Règles

Motifs glob comparés au chemin complet (séparateur `/`, y compris sous Windows ;
casse ignorée sous Windows). `*` traverse les dossiers.

| Motif | Effet |
|---|---|
| `*.tmp` | tous les `.tmp` |
| `*/Téléchargements/*` | tout ce qui est sous un dossier `Téléchargements` |
| `*/* (1).{jpg,png}` | copies « (1) » d'images |

- **Continue** : appliquée automatiquement à chaque doublon découvert ensuite
  pendant l'analyse.
- **Appliquer** : applique la règle maintenant à tous les résultats ; la règle
  reste dans la liste pour être réappliquée plus tard.
- **Retirer** : démarque les fichiers correspondants. **Tester** : compte les correspondances.

Une règle ne marque jamais le dernier exemplaire non marqué d'un groupe. Si une
règle continue a dû épargner un fichier pour cette raison, elle le marque dès
qu'un nouvel exemplaire (non concerné par la règle) est découvert.
Les règles et dossiers sont mémorisés entre deux lancements.

### Suppression

« Supprimer les fichiers marqués… » envoie à la corbeille (par défaut) ou
supprime définitivement. Avant chaque suppression, l'application vérifie
qu'un exemplaire conservé existe toujours et, par défaut, le compare octet
par octet au fichier à supprimer. Les groupes entièrement marqués sont ignorés.

### Liste de suppression différée

« Exporter la liste… » enregistre les fichiers marqués dans un fichier texte,
à exécuter plus tard avec « Exécuter une liste… » (aussi possible sans avoir
relancé d'analyse) :

```text
# conserver : /musique/album/03.flac
/copie/album/03.flac
/vieux/album/03.flac
```

Chaque bloc donne le ou les exemplaires conservés puis les fichiers à
supprimer ; les autres lignes `#` et les lignes vides sont ignorées, la liste
peut donc être modifiée à la main. À l'exécution, chaque fichier est revérifié :
un exemplaire conservé doit exister et être identique (octet par octet si la
vérification est active), et un fichier désigné comme conservé n'est jamais
supprimé. Un fichier sans ligne « conserver » n'est supprimé que si la
vérification est désactivée.

## Tests

```sh
cargo test
```
