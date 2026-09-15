//! Suppression des doublons, immédiate ou différée via une liste texte.
//!
//! Format de la liste (UTF-8, un chemin par ligne) :
//!
//! ```text
//! # commentaire libre
//! # conserver : /photos/img.jpg
//! /sauvegarde/img.jpg
//! /sauvegarde/vieux/img.jpg
//!
//! # conserver : /docs/a.pdf
//! /docs/a (1).pdf
//! ```
//!
//! Chaque bloc indique le ou les exemplaires conservés, puis les fichiers à
//! supprimer. Les autres lignes commençant par `#` et les lignes vides sont
//! ignorées, ce qui permet aussi d'utiliser la liste depuis un shell.

use std::collections::HashSet;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use eframe::egui;

const KEEP_PREFIX: &str = "# conserver : ";

/// Fichiers à supprimer et exemplaire(s) qui doivent rester.
#[derive(Clone, Debug, PartialEq)]
pub struct DeleteGroup {
    pub keepers: Vec<PathBuf>,
    pub targets: Vec<PathBuf>,
}

pub fn target_count(groups: &[DeleteGroup]) -> usize {
    groups.iter().map(|g| g.targets.len()).sum()
}

pub fn write_list(groups: &[DeleteGroup], out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "# Liste de suppression générée par Duplicate Finder")?;
    writeln!(out, "# {} fichier(s) à supprimer.", target_count(groups))?;
    writeln!(out, "# Blocs : lignes « {} » = exemplaire gardé, puis fichiers à supprimer.", KEEP_PREFIX.trim())?;
    writeln!(out, "# Exécution : Duplicate Finder > « Exécuter une liste… », qui revérifie chaque fichier.")?;
    writeln!(out, "# (Linux, sans vérification : grep -v -e '^#' -e '^$' liste.txt | xargs -d '\\n' rm --)")?;
    for g in groups {
        writeln!(out)?;
        for k in &g.keepers {
            writeln!(out, "{KEEP_PREFIX}{}", k.display())?;
        }
        for t in &g.targets {
            writeln!(out, "{}", t.display())?;
        }
    }
    Ok(())
}

pub fn read_list(input: impl BufRead) -> io::Result<Vec<DeleteGroup>> {
    let mut groups = Vec::new();
    let mut current = DeleteGroup { keepers: Vec::new(), targets: Vec::new() };
    let flush = |current: &mut DeleteGroup, groups: &mut Vec<DeleteGroup>| {
        if !current.targets.is_empty() {
            groups.push(std::mem::replace(current, DeleteGroup { keepers: Vec::new(), targets: Vec::new() }));
        } else {
            current.keepers.clear();
        }
    };
    for line in input.lines() {
        let line = line?;
        let line = line.strip_suffix('\r').unwrap_or(&line);
        if line.is_empty() {
            flush(&mut current, &mut groups);
        } else if let Some(keeper) = line.strip_prefix(KEEP_PREFIX) {
            if !current.targets.is_empty() {
                flush(&mut current, &mut groups);
            }
            current.keepers.push(PathBuf::from(keeper));
        } else if !line.starts_with('#') {
            current.targets.push(PathBuf::from(line));
        }
    }
    flush(&mut current, &mut groups);
    Ok(groups)
}

pub enum DeleteMsg {
    /// Chemin traité ; en cas de succès, taille libérée.
    Done(PathBuf, Result<u64, String>),
    Finished,
}

/// Supprime les fichiers en revérifiant chacun juste avant. À lancer dans un thread.
pub fn run(groups: Vec<DeleteGroup>, use_trash: bool, verify: bool, tx: Sender<DeleteMsg>, ctx: egui::Context) {
    // Un fichier désigné comme conservé n'est jamais supprimé, même si une
    // liste modifiée à la main le mentionne aussi comme cible.
    let all_keepers: HashSet<PathBuf> = groups.iter().flat_map(|g| g.keepers.iter().cloned()).collect();
    for group in groups {
        for target in &group.targets {
            let result = if all_keepers.contains(target) {
                Err("fichier désigné comme conservé, ignoré".to_string())
            } else {
                check(target, &group.keepers, verify).and_then(|size| {
                    let res = if use_trash { trash::delete(target).map_err(|e| e.to_string()) } else { std::fs::remove_file(target).map_err(|e| e.to_string()) };
                    res.map(|()| size)
                })
            };
            let result = result.map_err(|e| format!("{} : {e}", target.display()));
            if tx.send(DeleteMsg::Done(target.clone(), result)).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    }
    let _ = tx.send(DeleteMsg::Finished);
    ctx.request_repaint();
}

/// Vérifie qu'un exemplaire conservé existe, de même taille et, si demandé,
/// identique octet par octet. Sans exemplaire indiqué, la suppression n'est
/// autorisée que si la vérification est désactivée. Retourne la taille du fichier.
fn check(target: &Path, keepers: &[PathBuf], verify: bool) -> Result<u64, String> {
    let size = std::fs::metadata(target).map_err(|e| e.to_string())?.len();
    if keepers.is_empty() {
        return if verify {
            Err("aucun exemplaire conservé indiqué dans la liste, fichier ignoré (désactivez la vérification pour forcer)".into())
        } else {
            Ok(size)
        };
    }
    for k in keepers {
        if k == target {
            continue;
        }
        let same_size = std::fs::metadata(k).map(|m| m.len() == size).unwrap_or(false);
        if same_size && (!verify || files_equal(target, k).unwrap_or(false)) {
            return Ok(size);
        }
    }
    Err("aucun exemplaire conservé identique n'a été trouvé, fichier ignoré".into())
}

pub fn files_equal(a: &Path, b: &Path) -> io::Result<bool> {
    let mut fa = std::fs::File::open(a)?;
    let mut fb = std::fs::File::open(b)?;
    let mut ba = vec![0u8; 256 * 1024];
    let mut bb = vec![0u8; 256 * 1024];
    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}
