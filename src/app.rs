use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, RwLock};

use eframe::egui::{self, Color32, RichText};
use humansize::{format_size, DECIMAL};
use serde::{Deserialize, Serialize};

use crate::deletion::{self, DeleteGroup, DeleteMsg};
use crate::model::{FileId, GroupId, Model, NodeId, Status};
use crate::preview::Preview;
use crate::rules::{self, Exclusions, Rule, RuleSpec};
use crate::scanner::{self, ScanConfig, ScanEvent, ScanHandle, ScanProgress};

const SETTINGS_KEY: &str = "settings";
/// Nombre maximal d'événements de scan traités par image, pour rester fluide.
const EVENTS_PER_FRAME: usize = 20_000;
const MAX_ERRORS: usize = 10_000;

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    roots: Vec<PathBuf>,
    rules: Vec<RuleSpec>,
    exclusions: Vec<String>,
    min_size: u64,
    threads: usize,
    use_trash: bool,
    verify_before_delete: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Self {
            roots: Vec::new(),
            rules: Vec::new(),
            exclusions: Vec::new(),
            min_size: 1,
            threads: cores.min(4),
            use_trash: true,
            verify_before_delete: true,
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Tree,
    Groups,
}

struct DeleteJob {
    rx: Receiver<DeleteMsg>,
    total: usize,
    done: usize,
    ok: usize,
    freed: u64,
}

/// Suppression en attente de confirmation.
struct PendingDelete {
    /// Liste texte d'origine, ou `None` pour les fichiers marqués.
    list: Option<PathBuf>,
    groups: Vec<DeleteGroup>,
}

/// Aperçu en direct de l'effet d'un motif en cours de saisie, recalculé
/// seulement quand le motif ou les résultats changent.
#[derive(Default)]
struct LivePreview {
    key: Option<(String, u64)>,
    text: String,
    is_error: bool,
}

impl LivePreview {
    fn update(&mut self, pattern: &str, generation: u64, compute: impl FnOnce(&str) -> Result<String, String>) {
        let key = (pattern.to_string(), generation);
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.key = Some(key);
        let pattern = pattern.trim();
        (self.text, self.is_error) = if pattern.is_empty() {
            (String::new(), false)
        } else {
            match compute(pattern) {
                Ok(t) => (t, false),
                Err(e) => (e, true),
            }
        };
    }

    fn show(&self, ui: &mut egui::Ui) {
        if self.is_error {
            ui.colored_label(status_color(ui, Status::Delete), format!("Motif invalide : {}", self.text));
        } else if !self.text.is_empty() {
            ui.label(RichText::new(&self.text).italics());
        }
    }
}

enum RuleAction {
    Apply(usize),
    Unapply(usize),
    Test(usize),
    Remove(usize),
    ApplyAll,
}

pub struct App {
    settings: Settings,
    new_root: String,
    new_rule: String,
    new_rule_continuous: bool,
    rule_error: Option<String>,
    rule_preview: LivePreview,
    rules: Vec<Rule>,
    new_exclusion: String,
    exclusion_preview: LivePreview,
    exclusions: Arc<RwLock<Exclusions>>,
    model: Model,
    scan: Option<ScanHandle>,
    progress: ScanProgress,
    scan_status: String,
    errors: Vec<String>,
    show_errors: bool,
    selected: Option<FileId>,
    preview: Option<Preview>,
    tab: Tab,
    sorted_groups: Vec<GroupId>,
    sorted_generation: u64,
    pending_delete: Option<PendingDelete>,
    deleting: Option<DeleteJob>,
    /// Déplier / replier toute l'arborescence à la prochaine image.
    expand_tree: Option<bool>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_KEY))
            .unwrap_or_default();
        // Dossiers passés en ligne de commande.
        let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).filter(|p| p.is_dir()).collect();
        if !args.is_empty() {
            settings.roots = args;
        }
        Self::with_settings(settings)
    }

    fn with_settings(settings: Settings) -> Self {
        let rules = settings.rules.iter().filter_map(|r| Rule::new(r.clone()).ok()).collect();
        let mut settings = settings;
        settings.exclusions.retain(|p| Exclusions::new([p.as_str()]).is_ok());
        let exclusions = Exclusions::new(settings.exclusions.iter().map(String::as_str)).unwrap_or_default();
        Self {
            settings,
            new_root: String::new(),
            new_rule: String::new(),
            new_rule_continuous: true,
            rule_error: None,
            rule_preview: LivePreview::default(),
            rules,
            new_exclusion: String::new(),
            exclusion_preview: LivePreview::default(),
            exclusions: Arc::new(RwLock::new(exclusions)),
            model: Model::default(),
            scan: None,
            progress: ScanProgress::default(),
            scan_status: String::new(),
            errors: Vec::new(),
            show_errors: false,
            selected: None,
            preview: None,
            tab: Tab::Tree,
            sorted_groups: Vec::new(),
            sorted_generation: u64::MAX,
            pending_delete: None,
            deleting: None,
            expand_tree: None,
        }
    }

    fn push_error(&mut self, e: String) {
        if self.errors.len() < MAX_ERRORS {
            self.errors.push(e);
        }
    }

    // ------------------------------------------------------------------ scan

    fn start_scan(&mut self, ctx: &egui::Context) {
        if let Some(p) = self.preview.take() {
            p.release(ctx);
        }
        self.model.clear();
        self.selected = None;
        self.errors.clear();
        self.progress = ScanProgress { walking: true, ..Default::default() };
        self.scan_status = "Analyse en cours…".into();
        let config = ScanConfig {
            roots: self.settings.roots.clone(),
            min_size: self.settings.min_size,
            threads: self.settings.threads,
            exclusions: self.exclusions.clone(),
        };
        self.scan = Some(scanner::start(config, ctx.clone()));
    }

    fn poll_scan(&mut self, ctx: &egui::Context) {
        let Some(scan) = &self.scan else { return };
        let mut new_files = Vec::new();
        let mut errors = Vec::new();
        let mut finished = None;
        let mut processed = 0;
        while processed < EVENTS_PER_FRAME {
            let Ok(event) = scan.events.try_recv() else { break };
            processed += 1;
            match event {
                ScanEvent::NewGroup { key, size, files } => {
                    for path in files {
                        new_files.extend(self.model.add_file(key, size, path));
                    }
                }
                ScanEvent::AddToGroup { key, file } => {
                    new_files.extend(self.model.add_file(key, 0, file));
                }
                ScanEvent::Progress(p) => self.progress = p,
                ScanEvent::Error(e) => errors.push(e),
                ScanEvent::Finished { cancelled } => finished = Some(cancelled),
            }
        }
        if processed == EVENTS_PER_FRAME {
            ctx.request_repaint();
        }
        for e in errors {
            self.push_error(e);
        }
        if !new_files.is_empty() {
            let excl = self.exclusions.read().unwrap().clone();
            self.model.apply_exclusions_to(&excl, &new_files);
            rules::apply_continuous(&self.rules, &mut self.model, &new_files);
        }
        if let Some(cancelled) = finished {
            self.scan = None;
            self.scan_status = if cancelled { "Analyse interrompue".into() } else { "Analyse terminée".into() };
        }
    }

    // ----------------------------------------------------------- suppression

    /// Fichiers marqués regroupés avec leurs exemplaires conservés. Les groupes
    /// entièrement marqués sont ignorés.
    fn marked_groups(&self) -> Vec<DeleteGroup> {
        let m = &self.model;
        let path = |f: FileId| m.files[f].path.clone();
        m.groups
            .iter()
            .filter_map(|g| {
                let (targets, keepers): (Vec<FileId>, Vec<FileId>) = g.alive(&m.files).partition(|&f| m.files[f].marked);
                (!targets.is_empty() && !keepers.is_empty()).then(|| DeleteGroup {
                    keepers: keepers.into_iter().map(path).collect(),
                    targets: targets.into_iter().map(path).collect(),
                })
            })
            .collect()
    }

    fn start_delete(&mut self, groups: Vec<DeleteGroup>, ctx: &egui::Context) {
        let total = deletion::target_count(&groups);
        if total == 0 {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let (use_trash, verify) = (self.settings.use_trash, self.settings.verify_before_delete);
        let ctx = ctx.clone();
        self.deleting = Some(DeleteJob { rx, total, done: 0, ok: 0, freed: 0 });
        std::thread::spawn(move || deletion::run(groups, use_trash, verify, tx, ctx));
    }

    fn poll_delete(&mut self) {
        let Some(job) = &mut self.deleting else { return };
        let mut errors = Vec::new();
        let mut finished = false;
        while let Ok(msg) = job.rx.try_recv() {
            match msg {
                DeleteMsg::Done(path, res) => {
                    job.done += 1;
                    match res {
                        Ok(size) => {
                            job.ok += 1;
                            job.freed += size;
                            if let Some(id) = self.model.find(&path) {
                                self.model.mark_removed(id);
                            }
                        }
                        Err(e) => errors.push(e),
                    }
                }
                DeleteMsg::Finished => finished = true,
            }
        }
        if finished {
            self.scan_status = format!(
                "{} fichier(s) supprimé(s), {} libéré(s), {} échec(s)",
                job.ok,
                format_size(job.freed, DECIMAL),
                job.total - job.ok
            );
            self.deleting = None;
        }
        if !errors.is_empty() {
            self.show_errors = true;
        }
        for e in errors {
            self.push_error(e);
        }
    }

    fn export_list(&mut self) {
        let groups = self.marked_groups();
        let Some(path) = rfd::FileDialog::new()
            .set_file_name("liste-suppression.txt")
            .add_filter("Texte", &["txt"])
            .save_file()
        else {
            return;
        };
        let result = std::fs::File::create(&path).and_then(|f| {
            let mut w = std::io::BufWriter::new(f);
            deletion::write_list(&groups, &mut w)?;
            w.flush()
        });
        match result {
            Ok(()) => {
                self.scan_status =
                    format!("Liste exportée : {} fichier(s) → {}", deletion::target_count(&groups), path.display())
            }
            Err(e) => {
                self.push_error(format!("{} : {e}", path.display()));
                self.show_errors = true;
            }
        }
    }

    fn load_list(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("Texte", &["txt"]).pick_file() else { return };
        match std::fs::File::open(&path).map(std::io::BufReader::new).and_then(deletion::read_list) {
            Ok(groups) if deletion::target_count(&groups) == 0 => {
                self.scan_status = format!("{} : aucun fichier à supprimer", path.display());
            }
            Ok(groups) => self.pending_delete = Some(PendingDelete { list: Some(path), groups }),
            Err(e) => {
                self.push_error(format!("{} : {e}", path.display()));
                self.show_errors = true;
            }
        }
    }

    fn set_exclusions(&mut self, patterns: Vec<String>) {
        let excl = Exclusions::new(patterns.iter().map(String::as_str)).unwrap_or_default();
        self.settings.exclusions = patterns;
        self.model.apply_exclusions(&excl);
        *self.exclusions.write().unwrap() = excl;
    }

    // ------------------------------------------------------------- panneaux

    fn top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let scanning = self.scan.is_some();
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.strong("Dossiers :");
            let mut remove = None;
            for (i, root) in self.settings.roots.iter().enumerate() {
                ui.group(|ui| {
                    ui.label(root.display().to_string());
                    if ui.add_enabled(!scanning, egui::Button::new("✖").small()).on_hover_text("Retirer").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                self.settings.roots.remove(i);
            }
            if ui.add_enabled(!scanning, egui::Button::new("📁 Ajouter…")).clicked()
                && let Some(dirs) = rfd::FileDialog::new().pick_folders() {
                    for d in dirs {
                        if !self.settings.roots.contains(&d) {
                            self.settings.roots.push(d);
                        }
                    }
                }
            let edit = ui.add_enabled(
                !scanning,
                egui::TextEdit::singleline(&mut self.new_root).hint_text("ou saisir un chemin").desired_width(220.0),
            );
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.add_enabled(!scanning, egui::Button::new("+")).clicked() || enter) && !self.new_root.trim().is_empty() {
                let p = PathBuf::from(self.new_root.trim());
                if p.is_dir() {
                    if !self.settings.roots.contains(&p) {
                        self.settings.roots.push(p);
                    }
                    self.new_root.clear();
                } else {
                    self.push_error(format!("{} : dossier introuvable", p.display()));
                    self.show_errors = true;
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!scanning, |ui| {
                ui.label("Taille min. (octets) :");
                ui.add(egui::DragValue::new(&mut self.settings.min_size).range(1..=u64::MAX).speed(1024));
                ui.label("Threads de lecture :");
                ui.add(egui::DragValue::new(&mut self.settings.threads).range(1..=64))
                    .on_hover_text("Sur disque dur mécanique, 1 ou 2 threads sont plus rapides.");
            });
            ui.separator();
            if scanning {
                if ui.button("⏹ Arrêter").clicked()
                    && let Some(s) = &self.scan {
                        s.cancel();
                    }
                ui.spinner();
            } else if ui
                .add_enabled(!self.settings.roots.is_empty() && self.deleting.is_none(), egui::Button::new("▶ Lancer l'analyse"))
                .clicked()
            {
                self.start_scan(ctx);
            }
            let p = self.progress;
            if scanning || p.files_seen > 0 {
                ui.label(format!(
                    "{} {} fichiers ({}) · hachés : {} ({}) · en attente : {}",
                    if p.walking { "Parcours :" } else { "Parcourus :" },
                    p.files_seen,
                    format_size(p.bytes_seen, DECIMAL),
                    p.files_hashed,
                    format_size(p.bytes_hashed, DECIMAL),
                    p.pending_jobs,
                ));
            }
            if !self.scan_status.is_empty() {
                ui.weak(&self.scan_status);
            }
        });
        ui.add_space(4.0);
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        let t = self.model.totals;
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(format!(
                "{} groupes · {} fichiers · {} récupérables",
                self.model.visible_groups,
                t.total,
                format_size(self.model.wasted_bytes, DECIMAL)
            ));
            ui.separator();
            let deletable = t.delete - t.conflict;
            ui.colored_label(
                status_color(ui, Status::Delete),
                format!("À supprimer : {} ({})", deletable, format_size(t.delete_bytes, DECIMAL)),
            );
            ui.colored_label(status_color(ui, Status::Keep), format!("Conservés : {}", t.keep));
            if t.conflict > 0 {
                ui.colored_label(status_color(ui, Status::Conflict), format!("⚠ Tous exemplaires marqués : {}", t.conflict))
                    .on_hover_text("Ces fichiers ne seront pas supprimés : il faut conserver au moins un exemplaire par groupe.");
            }
            ui.separator();
            let (mut delete, mut export, mut load) = (false, false, false);
            if let Some(job) = &self.deleting {
                ui.spinner();
                ui.label(format!("Suppression {}/{}", job.done, job.total));
            } else {
                delete = ui
                    .add_enabled(deletable > 0, egui::Button::new(RichText::new("🗑 Supprimer les fichiers marqués…").color(status_color(ui, Status::Delete))))
                    .clicked();
                export = ui
                    .add_enabled(deletable > 0, egui::Button::new("📝 Exporter la liste…"))
                    .on_hover_text("Enregistre les fichiers marqués dans un fichier texte, pour les supprimer plus tard")
                    .clicked();
                load = ui
                    .button("📂 Exécuter une liste…")
                    .on_hover_text("Supprime les fichiers d'une liste exportée précédemment (chaque fichier est revérifié)")
                    .clicked();
            }
            if delete {
                self.pending_delete = Some(PendingDelete { list: None, groups: self.marked_groups() });
            }
            if export {
                self.export_list();
            }
            if load {
                self.load_list();
            }
            if !self.errors.is_empty() && ui.button(format!("⚠ Erreurs ({})", self.errors.len())).clicked() {
                self.show_errors = true;
            }
        });
        ui.add_space(4.0);
    }

    fn rules_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().id_salt("left-panel").auto_shrink([false, false]).show(ui, |ui| {
            self.exclusions_section(ui);
            ui.separator();
            self.rules_section(ui);
        });
    }

    fn exclusions_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Exclusions de la recherche");
        let edit = ui.add(
            egui::TextEdit::singleline(&mut self.new_exclusion)
                .hint_text("ex. */.git ou */AlbumArt*.jpg")
                .desired_width(f32::INFINITY),
        );
        let model = &self.model;
        self.exclusion_preview.update(&self.new_exclusion, model.generation, |p| {
            let excl = Exclusions::new([p])?;
            let c = rules::count_exclusions(&excl, model);
            Ok(if model.totals.total == 0 && model.files.is_empty() {
                "Aucun résultat pour l'instant : s'appliquera à la prochaine analyse".into()
            } else {
                format!("→ masquerait {} doublon(s) dans {} dossier(s)", c.files, c.folders)
            })
        });
        self.exclusion_preview.show(ui);
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let valid = !self.new_exclusion.trim().is_empty() && !self.exclusion_preview.is_error;
        if (ui.add_enabled(valid, egui::Button::new("➕ Exclure")).clicked() || (enter && valid))
            && !self.settings.exclusions.iter().any(|e| e == self.new_exclusion.trim())
        {
            let mut patterns = self.settings.exclusions.clone();
            patterns.push(self.new_exclusion.trim().to_string());
            self.set_exclusions(patterns);
            self.new_exclusion.clear();
        }
        let mut remove = None;
        for (i, pattern) in self.settings.exclusions.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.small_button("🗑").on_hover_text("Retirer l'exclusion").clicked() {
                    remove = Some(i);
                }
                ui.label(RichText::new(pattern).monospace());
            });
        }
        if let Some(i) = remove {
            let mut patterns = self.settings.exclusions.clone();
            patterns.remove(i);
            self.set_exclusions(patterns);
        }
        if !self.settings.exclusions.is_empty() {
            ui.weak("Les fichiers ignorés pendant le parcours ne réapparaissent qu'en relançant l'analyse.");
        }
    }

    fn rules_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Règles de suppression");
        ui.add(
            egui::TextEdit::singleline(&mut self.new_rule)
                .hint_text("ex. */sauvegarde/* ou *.bak")
                .desired_width(f32::INFINITY),
        );
        let model = &self.model;
        self.rule_preview.update(&self.new_rule, model.generation, |p| {
            let rule = Rule::new(RuleSpec { pattern: p.into(), continuous: false })?;
            let c = rules::count_rule(&rule, model);
            Ok(format!("→ {} doublon(s) dans {} dossier(s), {} seraient marqués", c.files, c.folders, c.markable))
        });
        self.rule_preview.show(ui);
        ui.checkbox(&mut self.new_rule_continuous, "Continue")
            .on_hover_text("Appliquée automatiquement aux doublons découverts ensuite pendant l'analyse.");
        ui.horizontal(|ui| {
            if ui.button("➕ Ajouter la règle").clicked() && !self.new_rule.trim().is_empty() {
                let spec = RuleSpec { pattern: self.new_rule.trim().to_string(), continuous: self.new_rule_continuous };
                match Rule::new(spec) {
                    Ok(r) => {
                        self.rules.push(r);
                        self.new_rule.clear();
                        self.rule_error = None;
                    }
                    Err(e) => self.rule_error = Some(e),
                }
            }
        });
        if let Some(e) = &self.rule_error {
            ui.colored_label(status_color(ui, Status::Delete), e);
        }
        egui::CollapsingHeader::new("Syntaxe").show(ui, |ui| {
            ui.label(
                "Le motif est comparé au chemin complet (séparateur « / »).\n\
                 • * : n'importe quels caractères, y compris « / »\n\
                 • ? : un caractère\n\
                 • [abc] : un caractère parmi a, b, c\n\
                 • {jpg,png} : une des alternatives\n\n\
                 Exemples :\n\
                 *.tmp\n\
                 */Téléchargements/*\n\
                 /home/moi/Photos/copie*/*\n\
                 */* (1).{jpg,png}",
            );
            ui.weak("Une règle ne marque jamais le dernier exemplaire non marqué d'un groupe.");
        });
        ui.separator();

        let mut action = None;
        if ui.add_enabled(!self.rules.is_empty(), egui::Button::new("Appliquer toutes les règles")).clicked() {
            action = Some(RuleAction::ApplyAll);
        }
        {
            for (i, rule) in self.rules.iter_mut().enumerate() {
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(&rule.spec.pattern).monospace().strong());
                    ui.checkbox(&mut rule.spec.continuous, "Continue");
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Appliquer").on_hover_text("Marque maintenant les doublons correspondants").clicked() {
                            action = Some(RuleAction::Apply(i));
                        }
                        if ui.button("Retirer").on_hover_text("Démarque les fichiers correspondants").clicked() {
                            action = Some(RuleAction::Unapply(i));
                        }
                        if ui.button("Tester").on_hover_text("Compte les doublons correspondants").clicked() {
                            action = Some(RuleAction::Test(i));
                        }
                        if ui.button("🗑").on_hover_text("Supprimer la règle").clicked() {
                            action = Some(RuleAction::Remove(i));
                        }
                    });
                    if let Some(r) = &rule.last_result {
                        ui.weak(r);
                    }
                });
            }
        }

        let all = 0..self.model.files.len();
        match action {
            Some(RuleAction::Apply(i)) => {
                let r = rules::apply(&self.rules[i], &mut self.model, all);
                self.rules[i].last_result = Some(apply_summary(&r));
            }
            Some(RuleAction::ApplyAll) => {
                for i in 0..self.rules.len() {
                    let r = rules::apply(&self.rules[i], &mut self.model, all.clone());
                    self.rules[i].last_result = Some(apply_summary(&r));
                }
            }
            Some(RuleAction::Unapply(i)) => {
                let n = rules::unapply(&self.rules[i], &mut self.model);
                self.rules[i].last_result = Some(format!("{n} fichier(s) démarqué(s)"));
            }
            Some(RuleAction::Test(i)) => {
                let n = rules::count_matches(&self.rules[i], &self.model);
                self.rules[i].last_result = Some(format!("{n} doublon(s) correspondant(s)"));
            }
            Some(RuleAction::Remove(i)) => {
                self.rules.remove(i);
            }
            None => {}
        }
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(sel) = self.selected else {
            ui.heading("Aperçu");
            ui.weak("Sélectionnez un fichier dans les résultats.");
            return;
        };
        let path = self.model.files[sel].path.clone();
        if self.preview.as_ref().map(|p| p.path != path).unwrap_or(true) {
            if let Some(old) = self.preview.take() {
                old.release(&ctx);
            }
            self.preview = Some(Preview::load(&path));
        }

        egui::ScrollArea::vertical().id_salt("preview-panel").auto_shrink([false, false]).show(ui, |ui| {
            ui.heading(file_name(&path));
            ui.add(egui::Label::new(RichText::new(path.display().to_string()).small()).wrap());
            if self.model.files[sel].removed {
                ui.colored_label(status_color(ui, Status::Delete), "Fichier supprimé");
                return;
            }
            let status = self.model.status(sel);
            let group = self.model.files[sel].group;
            ui.horizontal(|ui| {
                ui.label(format_size(self.model.groups[group].size, DECIMAL));
                ui.colored_label(status_color(ui, status), status_text(status));
            });
            ui.horizontal(|ui| {
                if ui.button("Ouvrir").clicked() {
                    open_path(&path, &mut self.errors);
                }
                if ui.button("Ouvrir le dossier").clicked()
                    && let Some(parent) = path.parent() {
                        open_path(parent, &mut self.errors);
                    }
            });

            ui.separator();
            let copies: Vec<FileId> = self.model.groups[group].alive(&self.model.files).collect();
            ui.strong(format!("Exemplaires identiques ({})", copies.len()));
            for c in copies {
                let st = self.model.status(c);
                ui.horizontal(|ui| {
                    let mut marked = self.model.files[c].marked;
                    if ui.checkbox(&mut marked, "").on_hover_text("Marquer pour suppression").changed() {
                        self.model.set_marked(c, marked);
                    }
                    if ui.small_button("Garder").on_hover_text("Conserver celui-ci et marquer tous les autres").clicked() {
                        self.model.keep_only(c);
                    }
                    let text = RichText::new(self.model.files[c].path.display().to_string()).color(status_color(ui, st));
                    if ui.add(egui::Button::selectable(c == sel, text).truncate()).clicked() {
                        self.selected = Some(c);
                    }
                });
            }
            ui.separator();
            if let Some(p) = &self.preview {
                p.show(ui);
            }
        });
    }

    fn results(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Tree, "🌲 Arborescence");
            ui.selectable_value(&mut self.tab, Tab::Groups, "📋 Groupes");
            ui.separator();
            if self.tab == Tab::Tree {
                if ui.button("Tout déplier").clicked() {
                    self.expand_tree = Some(true);
                }
                if ui.button("Tout replier").clicked() {
                    self.expand_tree = Some(false);
                }
                ui.separator();
            }
            legend(ui);
        });
        ui.separator();
        if self.model.totals.total == 0 {
            ui.centered_and_justified(|ui| {
                ui.weak(if self.scan.is_some() { "Recherche de doublons…" } else { "Aucun doublon à afficher." });
            });
            return;
        }
        match self.tab {
            Tab::Tree => {
                egui::ScrollArea::both().id_salt("tree").auto_shrink([false, false]).show(ui, |ui| {
                    let roots: Vec<NodeId> = self.model.roots.values().copied().collect();
                    for r in roots {
                        if self.model.nodes[r].counts.total > 0 {
                            tree_node(ui, &mut self.model, r, &mut self.selected, &mut self.errors, self.expand_tree);
                        }
                    }
                });
                self.expand_tree = None;
            }
            Tab::Groups => self.groups_list(ui),
        }
    }

    fn groups_list(&mut self, ui: &mut egui::Ui) {
        if self.sorted_generation != self.model.generation {
            self.sorted_generation = self.model.generation;
            let model = &self.model;
            let mut groups: Vec<(u64, GroupId)> = model
                .groups
                .iter()
                .enumerate()
                .filter_map(|(i, g)| {
                    let n = g.alive(&model.files).count() as u64;
                    (n >= 2).then(|| (g.size * (n - 1), i))
                })
                .collect();
            groups.sort_unstable_by(|a, b| b.cmp(a));
            self.sorted_groups = groups.into_iter().map(|(_, g)| g).collect();
        }
        let row_height = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical().id_salt("groups").auto_shrink([false, false]).show_rows(
            ui,
            row_height,
            self.sorted_groups.len(),
            |ui, range| {
                for &g in &self.sorted_groups[range] {
                    let group = &self.model.groups[g];
                    let alive: Vec<FileId> = group.alive(&self.model.files).collect();
                    let statuses: Vec<Status> = alive.iter().map(|&f| self.model.status(f)).collect();
                    let status = if statuses.contains(&Status::Conflict) {
                        Status::Conflict
                    } else if statuses.contains(&Status::Keep) {
                        Status::Keep
                    } else if statuses.contains(&Status::Delete) {
                        Status::Delete
                    } else {
                        Status::Pending
                    };
                    let is_selected = self.selected.map(|s| self.model.files[s].group == g).unwrap_or(false);
                    let text = format!(
                        "{} × {}   {}",
                        format_size(group.size, DECIMAL),
                        alive.len(),
                        file_name(&self.model.files[alive[0]].path)
                    );
                    let resp = ui.add(
                        egui::Button::selectable(is_selected, RichText::new(text).color(status_color(ui, status)))
                            .truncate()
                            .min_size(egui::vec2(ui.available_width(), row_height)),
                    );
                    if resp.clicked() {
                        self.selected = Some(alive[0]);
                    }
                }
            },
        );
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending_delete else { return };
        let count = deletion::target_count(&pending.groups);
        let without_keeper: usize = pending.groups.iter().filter(|g| g.keepers.is_empty()).map(|g| g.targets.len()).sum();
        let t = self.model.totals;
        let (mut confirm, mut cancel, mut export) = (false, false, false);
        let modal = egui::Modal::new(egui::Id::new("confirm-delete")).show(ctx, |ui| {
            ui.set_max_width(520.0);
            match &pending.list {
                Some(list) => {
                    ui.heading("Exécuter la liste de suppression ?");
                    ui.add(egui::Label::new(list.display().to_string()).wrap());
                    ui.label(format!("{count} fichier(s) à supprimer."));
                    if without_keeper > 0 {
                        ui.colored_label(
                            status_color(ui, Status::Conflict),
                            format!("{without_keeper} fichier(s) sans exemplaire conservé indiqué : ignorés si la vérification est active."),
                        );
                    }
                }
                None => {
                    ui.heading("Supprimer les fichiers marqués ?");
                    ui.label(format!("{count} fichier(s), {} au total.", format_size(t.delete_bytes, DECIMAL)));
                    if t.conflict > 0 {
                        ui.colored_label(
                            status_color(ui, Status::Conflict),
                            format!("{} fichier(s) ignoré(s) : tous les exemplaires de leur groupe sont marqués.", t.conflict),
                        );
                    }
                }
            }
            ui.checkbox(&mut self.settings.use_trash, "Envoyer à la corbeille (sinon suppression définitive)");
            ui.checkbox(&mut self.settings.verify_before_delete, "Vérifier octet par octet avant de supprimer")
                .on_hover_text("Compare chaque fichier avec l'exemplaire conservé juste avant sa suppression.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                confirm = ui.button(RichText::new("Supprimer").color(status_color(ui, Status::Delete))).clicked();
                if pending.list.is_none() {
                    export = ui.button("📝 Exporter la liste à la place…").clicked();
                }
                cancel = ui.button("Annuler").clicked();
            });
        });
        if confirm {
            let pending = self.pending_delete.take().unwrap();
            self.start_delete(pending.groups, ctx);
        } else if export {
            self.pending_delete = None;
            self.export_list();
        } else if cancel || modal.should_close() {
            self.pending_delete = None;
        }
    }

    fn errors_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_errors;
        egui::Window::new("Erreurs").open(&mut open).default_size([600.0, 300.0]).show(ctx, |ui| {
            if ui.button("Effacer").clicked() {
                self.errors.clear();
            }
            let row = ui.text_style_height(&egui::TextStyle::Body);
            egui::ScrollArea::both().auto_shrink([false, false]).show_rows(ui, row, self.errors.len(), |ui, range| {
                for e in &self.errors[range] {
                    ui.add(egui::Label::new(e).extend());
                }
            });
        });
        self.show_errors = open;
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.settings.rules = self.rules.iter().map(|r| r.spec.clone()).collect();
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }
}

impl App {
    fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.poll_scan(&ctx);
        self.poll_delete();
        self.model.refresh();

        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui, &ctx));
        egui::Panel::bottom("bottom").show(ui, |ui| self.bottom_bar(ui));
        egui::Panel::left("rules").resizable(true).default_size(300.0).show(ui, |ui| self.rules_panel(ui));
        egui::Panel::right("preview").resizable(true).default_size(420.0).show(ui, |ui| self.preview_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| self.results(ui));

        self.delete_dialog(&ctx);
        self.errors_window(&ctx);
    }
}

// ------------------------------------------------------------------ arbre

fn tree_node(
    ui: &mut egui::Ui,
    model: &mut Model,
    node: NodeId,
    selected: &mut Option<FileId>,
    errors: &mut Vec<String>,
    force_open: Option<bool>,
) {
    // Les chaînes de dossiers ne contenant qu'un sous-dossier sont fusionnées
    // (« /home/moi/Photos » plutôt que trois niveaux).
    let mut label = model.nodes[node].name.clone();
    let mut n = node;
    loop {
        let children = visible_children(model, n);
        if direct_file_count(model, n) == 0 && children.len() == 1 {
            n = children[0];
            if !label.ends_with(['/', '\\']) {
                label.push(std::path::MAIN_SEPARATOR);
            }
            label.push_str(&model.nodes[n].name);
        } else {
            break;
        }
    }

    let counts = model.nodes[n].counts;
    let status = model.node_status(n);
    let id = ui.make_persistent_id(("tree-node", n));
    let mut toggle = None;
    let mut keep_folder = false;
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
    if let Some(open) = force_open {
        state.set_open(open);
    }
    let header = state.show_header(ui, |ui| {
        let mut checked = counts.delete == counts.total;
        let indeterminate = counts.delete > 0 && !checked;
        if ui
            .add(egui::Checkbox::new(&mut checked, "").indeterminate(indeterminate))
            .on_hover_text("Marquer / démarquer tout le dossier")
            .changed()
        {
            toggle = Some(checked);
        }
        let resp = ui.add(egui::Label::new(RichText::new(&label).color(status_color(ui, status))).sense(egui::Sense::click()));
        resp.context_menu(|ui| {
            if ui.button("Conserver ce dossier (démarquer)").clicked() {
                keep_folder = true;
                ui.close();
            }
            if ui.button("Ouvrir le dossier").clicked() {
                open_path(&model.node_path(n), errors);
                ui.close();
            }
        });
        let mut info = format!("{} fichier(s)", counts.total);
        if counts.delete > 0 {
            info.push_str(&format!(" · {} marqué(s), {}", counts.delete, format_size(counts.delete_bytes, DECIMAL)));
        }
        ui.weak(info);
    });
    if let Some(mark) = toggle {
        model.set_marked_subtree(n, mark);
    }
    if keep_folder {
        model.set_marked_subtree(n, false);
    }
    header.body(|ui| {
        for c in visible_children(model, n) {
            tree_node(ui, model, c, selected, errors, force_open);
        }
        let files: Vec<FileId> = model.nodes[n].files.iter().copied().filter(|&f| model.is_visible(f)).collect();
        for f in files {
            file_row(ui, model, f, selected, errors);
        }
    });
}

fn file_row(ui: &mut egui::Ui, model: &mut Model, f: FileId, selected: &mut Option<FileId>, errors: &mut Vec<String>) {
    let status = model.status(f);
    let path = model.files[f].path.clone();
    let group = &model.groups[model.files[f].group];
    let copies = group.alive(&model.files).count();
    let size = group.size;
    ui.horizontal(|ui| {
        let mut marked = model.files[f].marked;
        if ui.checkbox(&mut marked, "").changed() {
            model.set_marked(f, marked);
        }
        let text = RichText::new(format!("📄 {}", file_name(&path))).color(status_color(ui, status));
        let resp = ui.selectable_label(*selected == Some(f), text);
        if resp.clicked() {
            *selected = Some(f);
        }
        resp.context_menu(|ui| {
            if ui.button("Conserver uniquement celui-ci").clicked() {
                model.keep_only(f);
                ui.close();
            }
            if ui.button("Ouvrir").clicked() {
                open_path(&path, errors);
                ui.close();
            }
            if ui.button("Ouvrir le dossier").clicked() {
                if let Some(p) = path.parent() {
                    open_path(p, errors);
                }
                ui.close();
            }
        });
        ui.weak(format!("{} · {} exemplaires", format_size(size, DECIMAL), copies));
    });
}

fn visible_children(model: &Model, n: NodeId) -> Vec<NodeId> {
    model.nodes[n].children.values().copied().filter(|&c| model.nodes[c].counts.total > 0).collect()
}

fn direct_file_count(model: &Model, n: NodeId) -> usize {
    let in_children: usize = model.nodes[n].children.values().map(|&c| model.nodes[c].counts.total).sum();
    model.nodes[n].counts.total - in_children
}

// ------------------------------------------------------------- utilitaires

fn open_path(path: &Path, errors: &mut Vec<String>) {
    if let Err(e) = open::that_detached(path) {
        errors.push(format!("{} : {e}", path.display()));
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

fn apply_summary(r: &rules::ApplyResult) -> String {
    let mut s = format!("{} fichier(s) marqué(s)", r.marked);
    if r.protected > 0 {
        s.push_str(&format!(", {} protégé(s) (dernier exemplaire)", r.protected));
    }
    s
}

fn status_color(ui: &egui::Ui, status: Status) -> Color32 {
    match status {
        Status::Pending => ui.visuals().text_color(),
        Status::Delete => Color32::from_rgb(225, 65, 65),
        Status::Keep => Color32::from_rgb(55, 175, 75),
        Status::Conflict => Color32::from_rgb(235, 145, 20),
    }
}

fn status_text(status: Status) -> &'static str {
    match status {
        Status::Pending => "À traiter",
        Status::Delete => "À supprimer",
        Status::Keep => "Conservé (tous ses doublons sont marqués)",
        Status::Conflict => "Tous les exemplaires sont marqués !",
    }
}

fn legend(ui: &mut egui::Ui) {
    for s in [Status::Pending, Status::Delete, Status::Keep, Status::Conflict] {
        let short = match s {
            Status::Pending => "à traiter",
            Status::Delete => "à supprimer",
            Status::Keep => "conservé",
            Status::Conflict => "tous marqués",
        };
        ui.colored_label(status_color(ui, s), format!("■ {short}"));
    }
}

#[cfg(test)]
mod ui_tests {
    use super::*;

    /// Fait tourner l'interface complète hors écran sur un vrai scan : dépliage
    /// de l'arbre, sélection (aperçu texte, image, binaire), onglet groupes,
    /// règles et fenêtre de confirmation.
    #[test]
    fn headless_frames() {
        let dir = std::env::temp_dir().join(format!("dupfinder-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["a", "b/c", "b/d"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
            std::fs::write(dir.join(sub).join("note.txt"), "hello\nworld").unwrap();
            std::fs::write(dir.join(sub).join("data.bin"), [0u8, 1, 2, 3, 255, 0, 7]).unwrap();
            std::fs::write(dir.join(sub).join("pic.png"), b"not really a png").unwrap();
        }
        let ctx = egui::Context::default();
        let mut app = App::with_settings(Settings { roots: vec![dir.clone()], ..Default::default() });
        app.rules.push(Rule::new(RuleSpec { pattern: "*/b/*".into(), continuous: true }).unwrap());
        let frame = |app: &mut App| {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.draw(ui));
            output.textures_delta.clear();
        };
        frame(&mut app);
        app.start_scan(&ctx);
        let start = std::time::Instant::now();
        while app.scan.is_some() {
            assert!(start.elapsed().as_secs() < 30, "scan bloqué");
            frame(&mut app);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        frame(&mut app);
        assert_eq!(app.model.visible_groups, 3);
        assert_eq!(app.model.totals.keep, 3);
        assert_eq!(app.model.totals.delete, 6);

        app.expand_tree = Some(true);
        frame(&mut app);
        for f in 0..app.model.files.len() {
            app.selected = Some(f);
            frame(&mut app);
            frame(&mut app);
        }
        app.tab = Tab::Groups;
        frame(&mut app);

        // Compteurs en direct pendant la saisie.
        app.new_rule = "*/b/*".into();
        app.new_exclusion = "*/d".into();
        frame(&mut app);
        assert_eq!(app.rule_preview.text, "→ 6 doublon(s) dans 2 dossier(s), 0 seraient marqués");
        assert_eq!(app.exclusion_preview.text, "→ masquerait 3 doublon(s) dans 1 dossier(s)");
        app.new_exclusion = "[".into();
        frame(&mut app);
        assert!(app.exclusion_preview.is_error);
        app.set_exclusions(vec!["*/d".into()]);
        frame(&mut app);
        assert_eq!(app.model.totals.total, 6);
        assert_eq!(app.model.totals.keep, 3);
        app.pending_delete = Some(PendingDelete { list: None, groups: app.marked_groups() });
        app.show_errors = true;
        frame(&mut app);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
