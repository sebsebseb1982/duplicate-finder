//! Moteur de règles : motifs glob qui ajoutent des fichiers à la liste de
//! suppression.

use std::path::Path;

use globset::{Glob, GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use std::collections::HashSet;

use crate::model::{FileId, Model};

#[derive(Serialize, Deserialize, Clone)]
pub struct RuleSpec {
    pub pattern: String,
    /// Appliquée automatiquement aux doublons découverts pendant le scan.
    pub continuous: bool,
}

pub struct Rule {
    pub spec: RuleSpec,
    matcher: GlobMatcher,
    /// Résultat de la dernière application manuelle, pour information.
    pub last_result: Option<String>,
}

impl Rule {
    pub fn new(spec: RuleSpec) -> Result<Self, String> {
        let matcher = build_glob(&spec.pattern)?.compile_matcher();
        Ok(Self { spec, matcher, last_result: None })
    }

    pub fn matches(&self, path: &Path) -> bool {
        self.matcher.is_match(normalize(path))
    }
}

/// Normalisation en `/` pour que les mêmes motifs fonctionnent sous Windows.
fn normalize(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// `*` traverse les séparateurs : `*.tmp` correspond à tout fichier `.tmp`,
/// `*/sauvegarde/*` à tout fichier situé sous un dossier `sauvegarde`.
/// Sous Windows, la casse est ignorée.
fn build_glob(pattern: &str) -> Result<Glob, String> {
    GlobBuilder::new(&pattern.replace('\\', "/"))
        .literal_separator(false)
        .case_insensitive(cfg!(windows))
        .backslash_escape(false)
        .build()
        .map_err(|e| e.to_string())
}

/// Résultat d'une application de règle.
#[derive(Default)]
pub struct ApplyResult {
    pub marked: usize,
    /// Fichiers correspondants mais laissés pour garder au moins un exemplaire.
    pub protected: usize,
}

/// Applique une règle aux fichiers donnés.
pub fn apply(rule: &Rule, model: &mut Model, files: impl IntoIterator<Item = FileId>) -> ApplyResult {
    let mut result = ApplyResult::default();
    for f in files {
        if !model.is_visible(f) || model.files[f].marked || !rule.matches(&model.files[f].path) {
            continue;
        }
        if model.mark_keeping_one(f) {
            result.marked += 1;
        } else {
            result.protected += 1;
        }
    }
    result
}

/// Applique les règles continues aux fichiers qui viennent d'être découverts.
///
/// Les fichiers déjà connus de leurs groupes qui avaient été épargnés (dernier
/// exemplaire non marqué) sont retentés : le nouvel arrivant peut désormais
/// servir d'exemplaire conservé.
pub fn apply_continuous(rules: &[Rule], model: &mut Model, new_files: &[FileId]) {
    if !rules.iter().any(|r| r.spec.continuous) {
        return;
    }
    let new_set: HashSet<FileId> = new_files.iter().copied().collect();
    let groups: HashSet<usize> = new_files.iter().map(|&f| model.files[f].group).collect();
    let mut candidates = new_files.to_vec();
    for g in groups {
        candidates.extend(
            model.groups[g]
                .files
                .iter()
                .copied()
                .filter(|f| model.files[*f].rule_pending && !new_set.contains(f)),
        );
    }
    for f in candidates {
        if !model.is_visible(f) || model.files[f].marked {
            continue;
        }
        let path = &model.files[f].path;
        if rules.iter().any(|r| r.spec.continuous && r.matches(path)) && !model.mark_keeping_one(f) {
            model.files[f].rule_pending = true;
        }
    }
}

/// Retire de la liste de suppression les fichiers correspondant à la règle.
pub fn unapply(rule: &Rule, model: &mut Model) -> usize {
    let mut n = 0;
    for f in 0..model.files.len() {
        if model.files[f].marked && rule.matches(&model.files[f].path) {
            model.set_marked(f, false);
            n += 1;
        }
    }
    n
}

pub fn count_matches(rule: &Rule, model: &Model) -> usize {
    (0..model.files.len())
        .filter(|&f| model.is_visible(f) && rule.matches(&model.files[f].path))
        .count()
}

/// Motifs exclus de la recherche.
///
/// Un motif exclut un fichier s'il correspond à son chemin, et un dossier
/// entier s'il correspond au chemin du dossier (avec ou sans « / » final) :
/// `*/node_modules`, `*/node_modules/*` et `*.flac` sont tous valides.
#[derive(Clone)]
pub struct Exclusions {
    set: GlobSet,
    empty: bool,
}

impl Default for Exclusions {
    fn default() -> Self {
        Self { set: GlobSet::empty(), empty: true }
    }
}

impl Exclusions {
    pub fn new<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        let mut builder = GlobSetBuilder::new();
        let mut empty = true;
        for p in patterns {
            builder.add(build_glob(p)?);
            empty = false;
        }
        Ok(Self { set: builder.build().map_err(|e| e.to_string())?, empty })
    }

    pub fn excludes_file(&self, path: &Path) -> bool {
        !self.empty && self.set.is_match(normalize(path))
    }

    pub fn excludes_dir(&self, path: &Path) -> bool {
        if self.empty {
            return false;
        }
        let mut s = normalize(path);
        if self.set.is_match(&s) {
            return true;
        }
        if !s.ends_with('/') {
            s.push('/');
        }
        self.set.is_match(s)
    }
}

/// Aperçu en direct de l'effet d'un motif sur les résultats actuels.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct PatternCount {
    pub files: usize,
    pub folders: usize,
    /// Pour une règle de suppression : fichiers qui seraient réellement marqués
    /// (le dernier exemplaire d'un groupe est toujours épargné).
    pub markable: usize,
}

/// Compte les doublons visibles et les dossiers touchés par une règle de suppression.
pub fn count_rule(rule: &Rule, model: &Model) -> PatternCount {
    let matching: Vec<FileId> = (0..model.files.len())
        .filter(|&f| model.is_visible(f) && rule.matches(&model.files[f].path))
        .collect();
    let folders: HashSet<usize> = matching.iter().map(|&f| model.files[f].node).collect();
    // Simulation groupe par groupe : combien peuvent être marqués sans vider un groupe.
    let mut by_group: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for &f in &matching {
        if !model.files[f].marked {
            *by_group.entry(model.files[f].group).or_default() += 1;
        }
    }
    let markable = by_group
        .iter()
        .map(|(&g, &n)| {
            let unmarked = model.groups[g].alive(&model.files).filter(|&o| !model.files[o].marked).count();
            n.min(unmarked.saturating_sub(1))
        })
        .sum();
    PatternCount { files: matching.len(), folders: folders.len(), markable }
}

/// Compte les doublons visibles et les dossiers qu'un jeu d'exclusions masquerait.
pub fn count_exclusions(excl: &Exclusions, model: &Model) -> PatternCount {
    let excluded_nodes = model.excluded_nodes(excl);
    let mut folders = HashSet::new();
    let mut files = 0;
    for f in 0..model.files.len() {
        if model.is_visible(f) && (excluded_nodes[model.files[f].node] || excl.excludes_file(&model.files[f].path)) {
            files += 1;
            folders.insert(model.files[f].node);
        }
    }
    PatternCount { files, folders: folders.len(), markable: 0 }
}
