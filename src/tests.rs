use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use eframe::egui;

use crate::model::{Model, Status};
use crate::rules::{self, Rule, RuleSpec};
use crate::scanner::{self, ScanConfig, ScanEvent};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dupfinder-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, content: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Lance un scan complet et remplit un modèle, en appliquant les règles continues.
fn scan_into_model(roots: Vec<PathBuf>, continuous: &[Rule]) -> Model {
    let handle = scanner::start(ScanConfig { roots, min_size: 1, threads: 3, exclusions: Default::default() }, egui::Context::default());
    let mut model = Model::default();
    loop {
        match handle.events.recv_timeout(Duration::from_secs(30)).expect("scan bloqué") {
            ScanEvent::NewGroup { key, size, files } => {
                let ids: Vec<_> = files.into_iter().filter_map(|p| model.add_file(key, size, p)).collect();
                rules::apply_continuous(continuous, &mut model, &ids);
            }
            ScanEvent::AddToGroup { key, file } => {
                let ids: Vec<_> = model.add_file(key, 0, file).into_iter().collect();
                rules::apply_continuous(continuous, &mut model, &ids);
            }
            ScanEvent::Finished { cancelled } => {
                assert!(!cancelled);
                break;
            }
            _ => {}
        }
    }
    model.refresh();
    model
}

fn id_of(model: &Model, suffix: &str) -> usize {
    (0..model.files.len())
        .find(|&f| model.files[f].path.to_string_lossy().replace('\\', "/").ends_with(suffix))
        .unwrap_or_else(|| panic!("{suffix} introuvable"))
}

#[test]
fn detects_only_binary_identical_files() {
    let dir = temp_dir("detect");
    let big: Vec<u8> = (0..500_000u32).map(|i| (i % 251) as u8).collect();
    let mut big_variant = big.clone();
    big_variant[250_000] ^= 1; // même taille, même début et fin, contenu différent
    write(&dir.join("a/small.txt"), b"bonjour");
    write(&dir.join("b/small copy.txt"), b"bonjour");
    write(&dir.join("b/other.txt"), b"bonsoir"); // même taille, contenu différent
    write(&dir.join("a/big.bin"), &big);
    write(&dir.join("b/big.bin"), &big);
    write(&dir.join("c/big.bin"), &big);
    write(&dir.join("c/big_variant.bin"), &big_variant);
    write(&dir.join("c/unique.dat"), b"unique content here");

    // Le dossier « a » est aussi passé en racine : il ne doit pas être compté deux fois.
    let model = scan_into_model(vec![dir.clone(), dir.join("a")], &[]);
    assert_eq!(model.visible_groups, 2);
    assert_eq!(model.totals.total, 5);
    let small = id_of(&model, "a/small.txt");
    let small_copy = id_of(&model, "b/small copy.txt");
    assert_eq!(model.files[small].group, model.files[small_copy].group);
    let big_group = model.files[id_of(&model, "c/big.bin")].group;
    assert_eq!(model.groups[big_group].files.len(), 3);
    assert_eq!(model.wasted_bytes, 7 + 2 * 500_000);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn statuses_and_folder_marking() {
    let dir = temp_dir("status");
    write(&dir.join("keep/x.txt"), b"xxxx");
    write(&dir.join("dup/x.txt"), b"xxxx");
    write(&dir.join("dup/sub/x.txt"), b"xxxx");
    write(&dir.join("keep/y.txt"), b"yyyyy");
    write(&dir.join("dup/y.txt"), b"yyyyy");
    let mut model = scan_into_model(vec![dir.clone()], &[]);

    let dup_node = model.files[id_of(&model, "dup/x.txt")].node;
    model.set_marked_subtree(dup_node, true);
    model.refresh();
    assert_eq!(model.status(id_of(&model, "dup/x.txt")), Status::Delete);
    assert_eq!(model.status(id_of(&model, "dup/sub/x.txt")), Status::Delete);
    assert_eq!(model.status(id_of(&model, "keep/x.txt")), Status::Keep);
    assert_eq!(model.node_status(dup_node), Status::Delete);
    assert_eq!(model.node_status(model.files[id_of(&model, "keep/x.txt")].node), Status::Keep);
    assert_eq!(model.deletable_files().len(), 3);

    // Marquer aussi « keep » : tous les exemplaires marqués → conflit, rien de supprimable.
    let keep_node = model.files[id_of(&model, "keep/x.txt")].node;
    model.set_marked_subtree(keep_node, true);
    model.refresh();
    assert_eq!(model.status(id_of(&model, "keep/x.txt")), Status::Conflict);
    assert!(model.deletable_files().is_empty());
    assert_eq!(model.totals.conflict, 5);

    // Suppression effective d'un fichier : son groupe (2 exemplaires) disparaît.
    model.set_marked_subtree(keep_node, false);
    let y = id_of(&model, "dup/y.txt");
    model.mark_removed(y);
    model.refresh();
    assert!(!model.is_visible(id_of(&model, "keep/y.txt")));
    assert_eq!(model.visible_groups, 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn rules_never_mark_last_copy() {
    let dir = temp_dir("rules");
    write(&dir.join("photos/img.jpg"), b"jpegdata");
    write(&dir.join("backup/img.jpg"), b"jpegdata");
    write(&dir.join("backup/old/img.jpg"), b"jpegdata");
    write(&dir.join("backup/doc.txt"), b"document");
    write(&dir.join("backup/doc (1).txt"), b"document");

    // Règle continue appliquée pendant le scan.
    let continuous = Rule::new(RuleSpec { pattern: "*/backup/*".into(), continuous: true }).unwrap();
    let mut model = scan_into_model(vec![dir.clone()], std::slice::from_ref(&continuous));
    assert_eq!(model.status(id_of(&model, "photos/img.jpg")), Status::Keep);
    assert_eq!(model.status(id_of(&model, "backup/img.jpg")), Status::Delete);
    assert_eq!(model.status(id_of(&model, "backup/old/img.jpg")), Status::Delete);
    // doc.txt et doc (1).txt sont tous deux sous backup : un seul est marqué.
    let docs = [id_of(&model, "backup/doc.txt"), id_of(&model, "backup/doc (1).txt")];
    assert_eq!(docs.iter().filter(|&&d| model.files[d].marked).count(), 1);
    assert!(model.deletable_files().iter().all(|&f| model.status(f) == Status::Delete));

    // Règle ponctuelle, retrait puis ré-application ultérieure.
    let unit = Rule::new(RuleSpec { pattern: "*(1).txt".into(), continuous: false }).unwrap();
    assert_eq!(rules::count_matches(&unit, &model), 1);
    for d in docs {
        model.set_marked(d, false);
    }
    let n = model.files.len();
    let r = rules::apply(&unit, &mut model, 0..n);
    assert_eq!(r.marked, 1);
    assert_eq!(rules::unapply(&unit, &mut model), 1);
    let n = model.files.len();
    let r = rules::apply(&unit, &mut model, 0..n);
    assert_eq!(r.marked, 1);
    model.refresh();
    assert_eq!(model.status(id_of(&model, "backup/doc.txt")), Status::Keep);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn glob_syntax() {
    let rule = |p: &str| Rule::new(RuleSpec { pattern: p.into(), continuous: false }).unwrap();
    assert!(rule("*.tmp").matches(Path::new("/a/b/c.tmp")));
    assert!(!rule("*.tmp").matches(Path::new("/a/b/c.tmpx")));
    assert!(rule("*/{jpg,png}/*").matches(Path::new("/x/png/y.bin")));
    assert!(rule("/home/*/Téléchargements/*").matches(Path::new("/home/moi/Téléchargements/a/b.zip")));
    assert!(Rule::new(RuleSpec { pattern: "[".into(), continuous: false }).is_err());
}

fn scan_with_exclusions(roots: Vec<PathBuf>, patterns: &[&str]) -> Model {
    use std::sync::{Arc, RwLock};
    let excl = Arc::new(RwLock::new(rules::Exclusions::new(patterns.iter().copied()).unwrap()));
    let handle = scanner::start(ScanConfig { roots, min_size: 1, threads: 2, exclusions: excl }, egui::Context::default());
    let mut model = Model::default();
    loop {
        match handle.events.recv_timeout(Duration::from_secs(30)).expect("scan bloqué") {
            ScanEvent::NewGroup { key, size, files } => {
                for p in files {
                    model.add_file(key, size, p);
                }
            }
            ScanEvent::AddToGroup { key, file } => {
                model.add_file(key, 0, file);
            }
            ScanEvent::Finished { .. } => break,
            _ => {}
        }
    }
    model.refresh();
    model
}

/// Les fichiers audio sont comparés en entier : une pochette identique
/// intégrée à deux morceaux différents n'en fait pas des doublons, et un jpg
/// identique à la pochette intégrée n'est pas le doublon du mp3.
#[test]
fn audio_files_compared_as_whole_files() {
    let dir = temp_dir("audio");
    let cover: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 256) as u8).collect();
    let track = |audio: u8| {
        let mut data = b"ID3\x04\x00\x00\x00\x00\x00\x00APIC".to_vec();
        data.extend_from_slice(&cover);
        data.extend(std::iter::repeat_n(audio, 300_000));
        data
    };
    write(&dir.join("album/01.mp3"), &track(1));
    write(&dir.join("album/02.mp3"), &track(2));
    write(&dir.join("album/cover.jpg"), &cover);
    write(&dir.join("album/03.flac"), &track(3));
    write(&dir.join("copie/03.flac"), &track(3));
    let model = scan_into_model(vec![dir.clone()], &[]);
    assert_eq!(model.visible_groups, 1, "seuls les deux flac identiques sont des doublons");
    assert!(model.files.iter().all(|f| f.path.extension().unwrap() == "flac"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn exclusions_during_scan_and_on_results() {
    let dir = temp_dir("excl");
    for sub in ["music/a", "music/b", "proj/.git/objects", "proj/src", "cache/x"] {
        write(&dir.join(sub).join("AlbumArt_{123}_Large.jpg"), b"art");
        write(&dir.join(sub).join("data.bin"), b"payload");
    }
    let all = scan_with_exclusions(vec![dir.clone()], &[]);
    assert_eq!(all.totals.total, 10);

    let model = scan_with_exclusions(vec![dir.clone()], &["*/AlbumArt*.jpg", "*/.git", "*/cache/*"]);
    assert_eq!(model.totals.total, 3, "seuls music/a, music/b et proj/src/data.bin restent");
    assert!(model.files.iter().all(|f| !f.path.to_string_lossy().contains(".git")));

    // Aperçu en direct puis application sur des résultats existants, et retour arrière.
    let mut model = all;
    let excl = rules::Exclusions::new(["*/proj"]).unwrap();
    let count = rules::count_exclusions(&excl, &model);
    assert_eq!((count.files, count.folders), (4, 2));
    model.apply_exclusions(&excl);
    model.refresh();
    assert_eq!(model.totals.total, 6);
    model.apply_exclusions(&rules::Exclusions::default());
    model.refresh();
    assert_eq!(model.totals.total, 10);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn live_rule_count() {
    let dir = temp_dir("count");
    write(&dir.join("keep/a.txt"), b"aaaa");
    write(&dir.join("bak/a.txt"), b"aaaa");
    write(&dir.join("bak/b.txt"), b"bbbbb");
    write(&dir.join("bak/old/b.txt"), b"bbbbb");
    let model = scan_into_model(vec![dir.clone()], &[]);
    let rule = Rule::new(RuleSpec { pattern: "*/bak/*".into(), continuous: false }).unwrap();
    let c = rules::count_rule(&rule, &model);
    // 3 fichiers dans 2 dossiers ; le groupe b est entièrement sous bak : un seul marquable.
    assert_eq!((c.files, c.folders, c.markable), (3, 2, 2));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn deletion_list_roundtrip_and_safety() {
    use crate::deletion::{self, DeleteGroup, DeleteMsg};
    let dir = temp_dir("list");
    let keep = dir.join("keep.txt");
    let dup = dir.join("dup copy.txt");
    let changed = dir.join("changed.txt");
    let protected = dir.join("protected.txt");
    write(&keep, b"same content");
    write(&dup, b"same content");
    write(&changed, b"same contenT"); // modifié depuis l'export
    write(&protected, b"same content");

    let groups = vec![
        DeleteGroup { keepers: vec![keep.clone()], targets: vec![dup.clone(), changed.clone()] },
        DeleteGroup { keepers: vec![protected.clone()], targets: vec![keep.clone()] },
    ];
    let mut text = Vec::new();
    deletion::write_list(&groups, &mut text).unwrap();
    // Édition à la main avec fins de ligne Windows et un bloc sans exemplaire conservé.
    let mut edited = String::from_utf8(text).unwrap().replace('\n', "\r\n");
    edited.push_str(&format!("\r\n# commentaire\r\n{}\r\n", protected.display()));
    let parsed = deletion::read_list(edited.as_bytes()).unwrap();
    assert_eq!(parsed[..2], groups[..]);
    assert_eq!(parsed[2], DeleteGroup { keepers: vec![], targets: vec![protected.clone()] });

    let (tx, rx) = std::sync::mpsc::channel();
    deletion::run(parsed, false, true, tx, egui::Context::default());
    let results: Vec<(PathBuf, bool)> = rx
        .iter()
        .filter_map(|m| match m {
            DeleteMsg::Done(p, r) => Some((p, r.is_ok())),
            DeleteMsg::Finished => None,
        })
        .collect();
    assert_eq!(
        results,
        vec![(dup.clone(), true), (changed.clone(), false), (keep.clone(), false), (protected.clone(), false)]
    );
    assert!(!dup.exists() && changed.exists() && keep.exists() && protected.exists());
    fs::remove_dir_all(dir).unwrap();
}
