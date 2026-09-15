//! Modèle des résultats : groupes de doublons, arborescence de dossiers et
//! état de marquage de chaque fichier.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

pub type FileId = usize;
pub type GroupId = usize;
pub type NodeId = usize;

/// État affiché d'un fichier ou d'un dossier.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// Gris : à traiter.
    Pending,
    /// Rouge : marqué pour suppression.
    Delete,
    /// Vert : exemplaire conservé, tous ses doublons sont marqués.
    Keep,
    /// Orange : tous les exemplaires du groupe sont marqués (rien ne sera supprimé).
    Conflict,
}

pub struct DupFile {
    pub path: PathBuf,
    pub group: GroupId,
    pub node: NodeId,
    pub marked: bool,
    /// Fichier supprimé du disque : il n'apparaît plus.
    pub removed: bool,
    /// Masqué par un motif d'exclusion (le fichier reste sur le disque).
    pub excluded: bool,
    /// Correspond à une règle continue mais a été épargné car c'était le
    /// dernier exemplaire non marqué ; retenté quand le groupe grandit.
    pub rule_pending: bool,
}

pub struct Group {
    pub size: u64,
    pub files: Vec<FileId>,
}

impl Group {
    pub fn alive<'a>(&'a self, files: &'a [DupFile]) -> impl Iterator<Item = FileId> + 'a {
        self.files.iter().copied().filter(|&f| !files[f].removed && !files[f].excluded)
    }
}

#[derive(Default, Clone, Copy)]
pub struct Counts {
    pub total: usize,
    pub delete: usize,
    pub keep: usize,
    pub conflict: usize,
    pub delete_bytes: u64,
}

pub struct Node {
    pub name: String,
    pub parent: Option<NodeId>,
    pub children: BTreeMap<String, NodeId>,
    pub files: Vec<FileId>,
    pub counts: Counts,
}

#[derive(Default)]
pub struct Model {
    pub files: Vec<DupFile>,
    pub groups: Vec<Group>,
    pub nodes: Vec<Node>,
    /// Racines de l'arborescence (une par lecteur / racine de système de fichiers).
    pub roots: BTreeMap<String, NodeId>,
    group_by_key: HashMap<u64, GroupId>,
    file_by_path: HashMap<PathBuf, FileId>,
    dirty: bool,
    pub totals: Counts,
    /// Groupes encore visibles (≥ 2 fichiers présents).
    pub visible_groups: usize,
    pub wasted_bytes: u64,
    /// Incrémenté à chaque recalcul, pour invalider les caches de l'interface.
    pub generation: u64,
}


fn component_name(c: Component<'_>) -> String {
    match c {
        Component::Prefix(p) => p.as_os_str().to_string_lossy().into_owned(),
        Component::RootDir => std::path::MAIN_SEPARATOR.to_string(),
        other => other.as_os_str().to_string_lossy().into_owned(),
    }
}

impl Model {
    pub fn clear(&mut self) {
        let generation = self.generation + 1;
        *self = Self::default();
        self.generation = generation;
    }

    /// Ajoute un fichier à un groupe (créé si besoin). Retourne l'identifiant
    /// du fichier ajouté, ou `None` s'il était déjà connu.
    pub fn add_file(&mut self, key: u64, size: u64, path: PathBuf) -> Option<FileId> {
        if self.file_by_path.contains_key(&path) {
            return None;
        }
        let group = *self.group_by_key.entry(key).or_insert_with(|| {
            self.groups.push(Group { size, files: Vec::new() });
            self.groups.len() - 1
        });
        let node = self.ensure_folder(path.parent().unwrap_or(Path::new("")));
        let id = self.files.len();
        self.files.push(DupFile { path: path.clone(), group, node, marked: false, removed: false, rule_pending: false, excluded: false });
        self.groups[group].files.push(id);
        self.nodes[node].files.push(id);
        self.file_by_path.insert(path, id);
        self.dirty = true;
        Some(id)
    }

    fn ensure_folder(&mut self, dir: &Path) -> NodeId {
        let mut current: Option<NodeId> = None;
        for comp in dir.components() {
            let name = component_name(comp);
            let existing = match current {
                None => self.roots.get(&name).copied(),
                Some(p) => self.nodes[p].children.get(&name).copied(),
            };
            let id = match existing {
                Some(id) => id,
                None => {
                    let id = self.nodes.len();
                    self.nodes.push(Node {
                        name: name.clone(),
                        parent: current,
                        children: BTreeMap::new(),
                        files: Vec::new(),
                        counts: Counts::default(),
                    });
                    match current {
                        None => self.roots.insert(name, id),
                        Some(p) => self.nodes[p].children.insert(name, id),
                    };
                    id
                }
            };
            current = Some(id);
        }
        current.expect("chemin de dossier vide")
    }

    pub fn node_path(&self, node: NodeId) -> PathBuf {
        let mut parts = Vec::new();
        let mut cur = Some(node);
        while let Some(n) = cur {
            parts.push(self.nodes[n].name.as_str());
            cur = self.nodes[n].parent;
        }
        parts.iter().rev().collect()
    }

    pub fn status(&self, file: FileId) -> Status {
        let f = &self.files[file];
        let group = &self.groups[f.group];
        let mut others = 0;
        let mut others_marked = 0;
        for o in group.alive(&self.files).filter(|&o| o != file) {
            others += 1;
            if self.files[o].marked {
                others_marked += 1;
            }
        }
        let all_others_marked = others > 0 && others == others_marked;
        match (f.marked, all_others_marked) {
            (true, true) => Status::Conflict,
            (true, false) => Status::Delete,
            (false, true) => Status::Keep,
            (false, false) => Status::Pending,
        }
    }

    pub fn is_visible(&self, file: FileId) -> bool {
        let f = &self.files[file];
        !f.removed && !f.excluded && self.groups[f.group].alive(&self.files).nth(1).is_some()
    }

    pub fn set_marked(&mut self, file: FileId, marked: bool) {
        self.files[file].rule_pending = false;
        if self.files[file].marked != marked {
            self.files[file].marked = marked;
            self.dirty = true;
        }
    }

    /// Marque ou démarque tous les fichiers visibles d'un sous-arbre.
    pub fn set_marked_subtree(&mut self, node: NodeId, marked: bool) {
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            stack.extend(self.nodes[n].children.values().copied());
            for i in 0..self.nodes[n].files.len() {
                let f = self.nodes[n].files[i];
                if self.is_visible(f) {
                    self.set_marked(f, marked);
                }
            }
        }
    }

    /// Conserve ce fichier et marque tous ses doublons.
    pub fn keep_only(&mut self, file: FileId) {
        let others: Vec<FileId> = self.groups[self.files[file].group].alive(&self.files).collect();
        for o in others {
            self.set_marked(o, o != file);
        }
    }

    /// Marque un fichier sauf si cela reviendrait à marquer tous les
    /// exemplaires de son groupe. Retourne `true` si le fichier a été marqué.
    pub fn mark_keeping_one(&mut self, file: FileId) -> bool {
        let f = &self.files[file];
        if f.marked || f.removed {
            return false;
        }
        let group = &self.groups[f.group];
        let unmarked_others = group
            .alive(&self.files)
            .filter(|&o| o != file && !self.files[o].marked)
            .count();
        if unmarked_others == 0 {
            return false;
        }
        self.set_marked(file, true);
        true
    }

    pub fn find(&self, path: &Path) -> Option<FileId> {
        self.file_by_path.get(path).copied()
    }

    /// Pour chaque dossier de l'arbre : exclu lui-même ou via un parent.
    pub fn excluded_nodes(&self, excl: &crate::rules::Exclusions) -> Vec<bool> {
        let mut result = vec![false; self.nodes.len()];
        // Un parent est toujours créé avant ses enfants : l'ordre des indices suffit.
        for n in 0..self.nodes.len() {
            let parent_excluded = self.nodes[n].parent.is_some_and(|p| result[p]);
            result[n] = parent_excluded || excl.excludes_dir(&self.node_path(n));
        }
        result
    }

    /// Masque les fichiers exclus et réaffiche ceux qui ne le sont plus.
    pub fn apply_exclusions(&mut self, excl: &crate::rules::Exclusions) {
        let nodes = self.excluded_nodes(excl);
        for f in 0..self.files.len() {
            let excluded = nodes[self.files[f].node] || excl.excludes_file(&self.files[f].path);
            if self.files[f].excluded != excluded {
                self.files[f].excluded = excluded;
                if excluded {
                    self.files[f].marked = false;
                }
                self.dirty = true;
            }
        }
    }

    /// Masque, parmi les fichiers donnés, ceux concernés par les exclusions
    /// (fichiers déjà en file de hachage quand une exclusion est ajoutée).
    pub fn apply_exclusions_to(&mut self, excl: &crate::rules::Exclusions, files: &[FileId]) {
        for &f in files {
            let mut excluded = excl.excludes_file(&self.files[f].path);
            let mut cur = Some(self.files[f].node);
            while let (false, Some(n)) = (excluded, cur) {
                excluded = excl.excludes_dir(&self.node_path(n));
                cur = self.nodes[n].parent;
            }
            if excluded {
                self.files[f].excluded = true;
                self.files[f].marked = false;
                self.dirty = true;
            }
        }
    }

    pub fn mark_removed(&mut self, file: FileId) {
        let f = &mut self.files[file];
        f.removed = true;
        f.marked = false;
        self.dirty = true;
    }

    /// Recalcule les compteurs agrégés si le modèle a changé.
    pub fn refresh(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.generation += 1;
        for n in &mut self.nodes {
            n.counts = Counts::default();
        }
        self.totals = Counts::default();
        for id in 0..self.files.len() {
            if !self.is_visible(id) {
                continue;
            }
            let status = self.status(id);
            let size = self.groups[self.files[id].group].size;
            let mut c = Counts { total: 1, ..Default::default() };
            match status {
                Status::Pending => {}
                Status::Delete => {
                    c.delete = 1;
                    c.delete_bytes = size;
                }
                Status::Keep => c.keep = 1,
                Status::Conflict => {
                    c.delete = 1;
                    c.conflict = 1;
                }
            }
            add_counts(&mut self.totals, c);
            let mut cur = Some(self.files[id].node);
            while let Some(n) = cur {
                add_counts(&mut self.nodes[n].counts, c);
                cur = self.nodes[n].parent;
            }
        }
        self.visible_groups = 0;
        self.wasted_bytes = 0;
        for g in &self.groups {
            let alive = g.alive(&self.files).count();
            if alive >= 2 {
                self.visible_groups += 1;
                self.wasted_bytes += g.size * (alive as u64 - 1);
            }
        }
    }

    /// Couleur agrégée d'un dossier.
    pub fn node_status(&self, node: NodeId) -> Status {
        let c = self.nodes[node].counts;
        if c.total == 0 {
            Status::Pending
        } else if c.conflict > 0 {
            Status::Conflict
        } else if c.delete == c.total {
            Status::Delete
        } else if c.keep == c.total {
            Status::Keep
        } else {
            Status::Pending
        }
    }

    #[cfg(test)]
    /// Fichiers réellement supprimables : marqués, et dont le groupe conserve
    /// au moins un exemplaire non marqué.
    pub fn deletable_files(&self) -> Vec<FileId> {
        (0..self.files.len())
            .filter(|&f| self.is_visible(f) && self.status(f) == Status::Delete)
            .collect()
    }
}

fn add_counts(a: &mut Counts, b: Counts) {
    a.total += b.total;
    a.delete += b.delete;
    a.keep += b.keep;
    a.conflict += b.conflict;
    a.delete_bytes += b.delete_bytes;
}
