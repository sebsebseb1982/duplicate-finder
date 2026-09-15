//! Moteur de recherche de doublons.
//!
//! Le scan est progressif : un thread parcourt les dossiers pendant qu'un pool
//! de threads hache les fichiers. Un fichier n'est lu que lorsqu'au moins un
//! autre fichier de même taille a été trouvé, puis on procède en deux temps :
//! empreinte partielle (début + fin du fichier) puis empreinte complète
//! (BLAKE3 256 bits) uniquement si les empreintes partielles coïncident.
//! Chaque groupe confirmé est envoyé immédiatement à l'interface.

#[cfg(unix)]
use std::collections::HashSet;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use eframe::egui;

use crate::rules::Exclusions;

/// En dessous de cette taille, l'empreinte partielle couvrirait tout le fichier :
/// on calcule directement l'empreinte complète.
const PARTIAL_CHUNK: u64 = 64 * 1024;

pub type Hash = [u8; 32];

#[derive(Clone)]
pub struct ScanConfig {
    pub roots: Vec<PathBuf>,
    pub min_size: u64,
    pub threads: usize,
    /// Partagé avec l'interface : une exclusion ajoutée pendant l'analyse
    /// s'applique au reste du parcours.
    pub exclusions: Arc<RwLock<Exclusions>>,
}

/// Événements envoyés à l'interface.
pub enum ScanEvent {
    /// Un nouveau groupe de doublons (au moins deux fichiers).
    NewGroup { key: u64, size: u64, files: Vec<PathBuf> },
    /// Un fichier supplémentaire rejoint un groupe existant.
    AddToGroup { key: u64, file: PathBuf },
    Progress(ScanProgress),
    Error(String),
    Finished { cancelled: bool },
}

#[derive(Clone, Copy, Default)]
pub struct ScanProgress {
    pub files_seen: u64,
    pub bytes_seen: u64,
    pub files_hashed: u64,
    pub bytes_hashed: u64,
    pub pending_jobs: u64,
    pub walking: bool,
}

pub struct ScanHandle {
    pub events: Receiver<ScanEvent>,
    cancel: Arc<AtomicBool>,
}

impl ScanHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

enum Job {
    Partial(usize, PathBuf, u64),
    Full(usize, PathBuf),
}

enum Msg {
    Found(PathBuf, u64),
    Partial(usize, Hash),
    Full(usize, Hash, u64),
    ReadError(usize, String),
    WalkError(String),
    WalkDone,
}

pub fn start(config: ScanConfig, ctx: egui::Context) -> ScanHandle {
    let (event_tx, event_rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel2 = cancel.clone();
    thread::Builder::new()
        .name("scan-coordinator".into())
        .spawn(move || coordinator(config, event_tx, cancel2, ctx))
        .expect("impossible de lancer le thread de scan");
    ScanHandle { events: event_rx, cancel }
}

/// Supprime les racines incluses dans une autre racine (sinon un même fichier
/// serait vu deux fois et considéré comme son propre doublon).
pub fn normalize_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut canon: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| dunce_canonicalize(r))
        .collect();
    canon.sort();
    canon.dedup();
    let mut result: Vec<PathBuf> = Vec::new();
    for r in canon {
        if !result.iter().any(|kept| r.starts_with(kept)) {
            result.push(r);
        }
    }
    result
}

/// `canonicalize` sous Windows produit des chemins `\\?\C:\...` peu lisibles
/// et mal gérés par certains outils : on retire ce préfixe quand c'est possible.
fn dunce_canonicalize(p: &Path) -> Option<PathBuf> {
    let c = std::fs::canonicalize(p).ok()?;
    #[cfg(windows)]
    {
        let s = c.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\")
            && !stripped.starts_with("UNC") {
                return Some(PathBuf::from(stripped));
            }
    }
    Some(c)
}

fn coordinator(config: ScanConfig, events: Sender<ScanEvent>, cancel: Arc<AtomicBool>, ctx: egui::Context) {
    let send = |e: ScanEvent| {
        let _ = events.send(e);
        ctx.request_repaint();
    };

    let roots = normalize_roots(&config.roots);
    if roots.is_empty() {
        send(ScanEvent::Error("Aucun dossier valide à analyser".into()));
        send(ScanEvent::Finished { cancelled: false });
        return;
    }

    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let job_rx = Arc::new(Mutex::new(job_rx));

    // Pool de hachage.
    let threads = config.threads.max(1);
    for i in 0..threads {
        let job_rx = job_rx.clone();
        let msg_tx = msg_tx.clone();
        let cancel = cancel.clone();
        thread::Builder::new()
            .name(format!("hash-{i}"))
            .spawn(move || hash_worker(job_rx, msg_tx, cancel))
            .expect("impossible de lancer un thread de hachage");
    }

    // Parcours des dossiers.
    {
        let msg_tx = msg_tx.clone();
        let cancel = cancel.clone();
        let min_size = config.min_size.max(1);
        let exclusions = config.exclusions.clone();
        thread::Builder::new()
            .name("walker".into())
            .spawn(move || walker(roots, min_size, exclusions, msg_tx, cancel))
            .expect("impossible de lancer le parcours");
    }
    drop(msg_tx);

    let mut paths: Vec<PathBuf> = Vec::new();
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut by_partial: HashMap<(u64, Hash), Vec<usize>> = HashMap::new();
    let mut by_full: HashMap<(u64, Hash), (u64, Vec<usize>)> = HashMap::new();
    #[cfg(unix)]
    let mut inodes: HashSet<(u64, u64)> = HashSet::new();
    let mut sizes: Vec<u64> = Vec::new();
    let mut next_group_key: u64 = 0;
    let mut progress = ScanProgress { walking: true, ..Default::default() };
    let mut pending: u64 = 0;
    let mut last_progress = std::time::Instant::now();

    let submit = |job: Job, pending: &mut u64| {
        *pending += 1;
        let _ = job_tx.send(job);
    };

    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if !progress.walking && pending == 0 {
            break;
        }
        let msg = match msg_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(m) => m,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                progress.pending_jobs = pending;
                send(ScanEvent::Progress(progress));
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match msg {
            Msg::Found(path, size) => {
                #[cfg(unix)]
                {
                    // Deux chemins vers le même inode (lien physique, montage
                    // lié...) désignent les mêmes données, pas une copie :
                    // via un montage lié, « supprimer le doublon » effacerait
                    // l'original. Seul le premier chemin rencontré est gardé.
                    use std::os::unix::fs::MetadataExt;
                    if let Ok(md) = std::fs::metadata(&path)
                        && !inodes.insert((md.dev(), md.ino()))
                    {
                        continue;
                    }
                }
                progress.files_seen += 1;
                progress.bytes_seen += size;
                let id = paths.len();
                paths.push(path);
                sizes.push(size);
                let bucket = by_size.entry(size).or_default();
                bucket.push(id);
                let to_start: Vec<usize> = match bucket.len() {
                    1 => vec![],
                    2 => bucket.clone(),
                    _ => vec![id],
                };
                for fid in to_start {
                    let job = if size <= PARTIAL_CHUNK * 2 {
                        Job::Full(fid, paths[fid].clone())
                    } else {
                        Job::Partial(fid, paths[fid].clone(), size)
                    };
                    submit(job, &mut pending);
                }
            }
            Msg::Partial(id, hash) => {
                pending -= 1;
                let bucket = by_partial.entry((sizes[id], hash)).or_default();
                bucket.push(id);
                let to_start: Vec<usize> = match bucket.len() {
                    1 => vec![],
                    2 => bucket.clone(),
                    _ => vec![id],
                };
                for fid in to_start {
                    submit(Job::Full(fid, paths[fid].clone()), &mut pending);
                }
            }
            Msg::Full(id, hash, bytes) => {
                pending -= 1;
                progress.files_hashed += 1;
                progress.bytes_hashed += bytes;
                let size = sizes[id];
                if bytes != size {
                    send(ScanEvent::Error(format!(
                        "{} : fichier modifié pendant l'analyse, ignoré",
                        paths[id].display()
                    )));
                    continue;
                }
                let entry = by_full.entry((size, hash)).or_insert_with(|| {
                    let k = next_group_key;
                    next_group_key += 1;
                    (k, Vec::new())
                });
                entry.1.push(id);
                match entry.1.len() {
                    1 => {}
                    2 => send(ScanEvent::NewGroup {
                        key: entry.0,
                        size,
                        files: entry.1.iter().map(|&f| paths[f].clone()).collect(),
                    }),
                    _ => send(ScanEvent::AddToGroup { key: entry.0, file: paths[id].clone() }),
                }
            }
            Msg::ReadError(id, err) => {
                pending -= 1;
                send(ScanEvent::Error(format!("{} : {err}", paths[id].display())));
            }
            Msg::WalkError(err) => send(ScanEvent::Error(err)),
            Msg::WalkDone => progress.walking = false,
        }
        if last_progress.elapsed().as_millis() > 100 {
            last_progress = std::time::Instant::now();
            progress.pending_jobs = pending;
            send(ScanEvent::Progress(progress));
        }
    }

    // Arrêt des workers : fermer le canal de travaux les fait sortir.
    drop(job_tx);
    progress.walking = false;
    progress.pending_jobs = 0;
    send(ScanEvent::Progress(progress));
    send(ScanEvent::Finished { cancelled: cancel.load(Ordering::Relaxed) });
}

fn walker(roots: Vec<PathBuf>, min_size: u64, exclusions: Arc<RwLock<Exclusions>>, tx: Sender<Msg>, cancel: Arc<AtomicBool>) {
    let excluded = |e: &walkdir::DirEntry| {
        let excl = exclusions.read().unwrap();
        if e.file_type().is_dir() {
            e.depth() > 0 && excl.excludes_dir(e.path())
        } else {
            excl.excludes_file(e.path())
        }
    };
    for root in roots {
        let entries = walkdir::WalkDir::new(&root).follow_links(false).into_iter().filter_entry(|e| !excluded(e));
        for entry in entries {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            match entry {
                Ok(e) => {
                    // Les liens symboliques sont ignorés : ce ne sont pas des copies.
                    if !e.file_type().is_file() {
                        continue;
                    }
                    match e.metadata() {
                        Ok(md) if md.len() >= min_size => {
                            let _ = tx.send(Msg::Found(e.into_path(), md.len()));
                        }
                        Ok(_) => {}
                        Err(err) => {
                            let _ = tx.send(Msg::WalkError(err.to_string()));
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(Msg::WalkError(err.to_string()));
                }
            }
        }
    }
    let _ = tx.send(Msg::WalkDone);
}

fn hash_worker(jobs: Arc<Mutex<Receiver<Job>>>, tx: Sender<Msg>, cancel: Arc<AtomicBool>) {
    loop {
        let job = {
            let rx = jobs.lock().unwrap();
            match rx.recv() {
                Ok(j) => j,
                Err(_) => return,
            }
        };
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let msg = match job {
            Job::Partial(id, path, size) => match partial_hash(&path, size) {
                Ok(h) => Msg::Partial(id, h),
                Err(e) => Msg::ReadError(id, e.to_string()),
            },
            Job::Full(id, path) => match full_hash(&path, &cancel) {
                Ok((h, n)) => Msg::Full(id, h, n),
                Err(e) => Msg::ReadError(id, e.to_string()),
            },
        };
        if tx.send(msg).is_err() {
            return;
        }
    }
}

fn partial_hash(path: &Path, size: u64) -> std::io::Result<Hash> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; PARTIAL_CHUNK as usize];
    let mut hasher = blake3::Hasher::new();
    f.read_exact(&mut buf)?;
    hasher.update(&buf);
    f.seek(SeekFrom::Start(size - PARTIAL_CHUNK))?;
    f.read_exact(&mut buf)?;
    hasher.update(&buf);
    Ok(*hasher.finalize().as_bytes())
}

fn full_hash(path: &Path, cancel: &AtomicBool) -> std::io::Result<(Hash, u64)> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut hasher = blake3::Hasher::new();
    let mut total = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("annulé"));
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((*hasher.finalize().as_bytes(), total))
}
