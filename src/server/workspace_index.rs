use ignore::{
    WalkBuilder,
    types::{Types, TypesBuilder},
};
use lsp_types::{Position, Range, Url};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
};
use tree_sitter_traversal2::{Order, traverse};

use crate::{
    response_item::{Item, ItemKind},
    server::Server,
    server::code_helper::IdentityCache,
    utils::*,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SymbolKindTag {
    Variable,
    Function,
    Module,
    Keyword,
}

impl SymbolKindTag {
    fn from_item_kind(kind: &ItemKind) -> Self {
        match kind {
            ItemKind::Variable => Self::Variable,
            ItemKind::Function { .. } => Self::Function,
            ItemKind::Module { .. } => Self::Module,
            ItemKind::Keyword => Self::Keyword,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PositionKey {
    pub(crate) line: u32,
    pub(crate) character: u32,
}

impl From<&Position> for PositionKey {
    fn from(position: &Position) -> Self {
        Self {
            line: position.line,
            character: position.character,
        }
    }
}

impl From<Position> for PositionKey {
    fn from(position: Position) -> Self {
        Self::from(&position)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RangeKey {
    pub(crate) start: PositionKey,
    pub(crate) end: PositionKey,
}

impl From<&Range> for RangeKey {
    fn from(range: &Range) -> Self {
        Self {
            start: PositionKey::from(&range.start),
            end: PositionKey::from(&range.end),
        }
    }
}

impl From<Range> for RangeKey {
    fn from(range: Range) -> Self {
        Self::from(&range)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SymbolKey {
    pub(crate) url: Url,
    pub(crate) name: String,
    pub(crate) kind: SymbolKindTag,
    pub(crate) range: RangeKey,
    pub(crate) is_top_level: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ResolvedSymbol {
    Workspace(SymbolKey),
    Builtin { name: String, kind: SymbolKindTag },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Occurrence {
    pub(crate) url: Url,
    pub(crate) range: Range,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FileAnalysis {
    pub(crate) dependencies: Vec<Url>,
    pub(crate) occurrence_map: HashMap<RangeKey, ResolvedSymbol>,
    pub(crate) resolved_occurrences: Vec<(SymbolKey, Occurrence)>,
}

#[derive(Default)]
pub(crate) struct WorkspaceIndex {
    pub(crate) files: HashMap<Url, FileAnalysis>,
    pub(crate) refs_by_symbol: HashMap<SymbolKey, Vec<Occurrence>>,
    pub(crate) reverse_deps: HashMap<Url, HashSet<Url>>,
    pub(crate) initialized: bool,
}

impl WorkspaceIndex {
    pub(crate) fn clear(&mut self) {
        self.files.clear();
        self.refs_by_symbol.clear();
        self.reverse_deps.clear();
        self.initialized = false;
    }

    pub(crate) fn symbol_at(&self, url: &Url, range: &Range) -> Option<ResolvedSymbol> {
        self.files
            .get(url)?
            .occurrence_map
            .get(&RangeKey::from(range))
            .cloned()
    }

    pub(crate) fn references_for(&self, symbol: &SymbolKey) -> Vec<Occurrence> {
        self.refs_by_symbol.get(symbol).cloned().unwrap_or_default()
    }

    pub(crate) fn collect_dependents(&self, url: &Url) -> HashSet<Url> {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(url.clone());

        while let Some(current) = queue.pop_front() {
            if let Some(dependents) = self.reverse_deps.get(&current) {
                for dependent in dependents {
                    if visited.insert(dependent.clone()) {
                        queue.push_back(dependent.clone());
                    }
                }
            }
        }

        visited
    }

    pub(crate) fn remove_file(&mut self, url: &Url) {
        let Some(analysis) = self.files.remove(url) else {
            return;
        };

        for dependency in analysis.dependencies {
            let should_remove = if let Some(users) = self.reverse_deps.get_mut(&dependency) {
                users.remove(url);
                users.is_empty()
            } else {
                false
            };
            if should_remove {
                self.reverse_deps.remove(&dependency);
            }
        }

        for (symbol, occurrence) in analysis.resolved_occurrences {
            let should_remove = if let Some(occurrences) = self.refs_by_symbol.get_mut(&symbol) {
                occurrences.retain(|existing| existing != &occurrence);
                occurrences.is_empty()
            } else {
                false
            };
            if should_remove {
                self.refs_by_symbol.remove(&symbol);
            }
        }
    }

    pub(crate) fn insert_file(&mut self, url: Url, analysis: FileAnalysis) {
        for dependency in &analysis.dependencies {
            self.reverse_deps
                .entry(dependency.clone())
                .or_default()
                .insert(url.clone());
        }

        for (symbol, occurrence) in &analysis.resolved_occurrences {
            self.refs_by_symbol
                .entry(symbol.clone())
                .or_default()
                .push(occurrence.clone());
        }

        self.files.insert(url, analysis);
    }
}

impl Server {
    pub(crate) fn clear_workspace_index(&mut self) {
        self.workspace_index.clear();
    }

    pub(crate) fn rebuild_workspace_index(&mut self) {
        for code in self.codes.values() {
            code.borrow_mut().changed = true;
        }
        self.clear_workspace_index();
        self.ensure_workspace_index();
    }

    pub(crate) fn ensure_workspace_index(&mut self) {
        let urls = self.collect_workspace_seed_urls();
        let pending: Vec<Url> = urls
            .into_iter()
            .filter(|url| !self.workspace_index.files.contains_key(url))
            .collect();

        if !pending.is_empty() {
            self.reindex_urls(pending);
        }

        self.workspace_index.initialized = true;
    }

    pub(crate) fn refresh_workspace_index_for_url(&mut self, url: &Url) {
        if !self.workspace_index.initialized {
            return;
        }

        let mut affected = self.workspace_index.collect_dependents(url);
        affected.insert(url.clone());
        self.reindex_urls(affected);
    }

    fn collect_workspace_seed_urls(&self) -> HashSet<Url> {
        let mut urls: HashSet<Url> = self
            .codes
            .keys()
            .filter(|url| **url != self.builtin_url)
            .cloned()
            .collect();

        let types = Self::scad_types();
        for root in &self.workspace_roots {
            Self::scan_directory_for_scad_files(root, &types, &mut urls);
        }

        urls
    }

    fn reindex_urls<I>(&mut self, urls: I)
    where
        I: IntoIterator<Item = Url>,
    {
        let mut pending: Vec<Url> = urls.into_iter().collect();
        pending.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        pending.dedup();

        for url in pending {
            let analysis = self.analyze_file(&url);
            self.workspace_index.remove_file(&url);
            if let Some(analysis) = analysis {
                self.workspace_index.insert_file(url, analysis);
            }
        }
    }

    fn analyze_file(&mut self, url: &Url) -> Option<FileAnalysis> {
        let code_rc = self.get_code(url)?;
        if let Ok(mut code_mut) = code_rc.try_borrow_mut() {
            code_mut.gen_top_level_items_if_needed();
        }

        let code_ref = code_rc.borrow();
        let mut dependencies = code_ref.includes.clone().unwrap_or_default();
        if let Some(uses) = &code_ref.uses {
            dependencies.extend(uses.iter().cloned());
        }
        dependencies.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        dependencies.dedup();
        let mut occurrence_map = HashMap::new();
        let mut resolved_occurrences = Vec::new();
        let mut name_caches: HashMap<String, IdentityCache> = HashMap::new();

        let cursor = code_ref.tree.walk();
        let iter = traverse(cursor, Order::Pre);
        for node in iter {
            if node.kind() != "identifier" {
                continue;
            }

            let name = node_text(&code_ref.code, &node).to_owned();
            let cache = name_caches.entry(name.clone()).or_default();
            let resolved = self.find_identities_with_cache(
                &code_ref,
                &|item_name| item_name == name.as_str(),
                &node,
                false,
                cache,
            );
            let Some(item) = resolved.first() else {
                continue;
            };

            let range = Range {
                start: to_position(node.start_position()),
                end: to_position(node.end_position()),
            };
            let Some(symbol) = Self::resolved_symbol_from_item(&item.borrow()) else {
                continue;
            };

            occurrence_map.insert(RangeKey::from(&range), symbol.clone());

            if let ResolvedSymbol::Workspace(symbol_key) = symbol {
                resolved_occurrences.push((
                    symbol_key,
                    Occurrence {
                        url: url.clone(),
                        range,
                    },
                ));
            }
        }

        Some(FileAnalysis {
            dependencies,
            occurrence_map,
            resolved_occurrences,
        })
    }

    pub(crate) fn resolved_symbol_from_item(item: &Item) -> Option<ResolvedSymbol> {
        let kind = SymbolKindTag::from_item_kind(&item.kind);
        if let Some(url) = &item.url {
            Some(ResolvedSymbol::Workspace(SymbolKey {
                url: url.clone(),
                name: item.name.clone(),
                kind,
                range: RangeKey::from(&item.range),
                is_top_level: item.is_top_level,
            }))
        } else {
            Some(ResolvedSymbol::Builtin {
                name: item.name.clone(),
                kind,
            })
        }
    }

    fn scad_types() -> Types {
        let mut builder = TypesBuilder::new();
        builder.add("scad", "*.scad").expect("valid glob");
        builder.select("scad");
        builder.build().expect("build types")
    }

    fn scan_directory_for_scad_files(dir: &Path, types: &Types, urls: &mut HashSet<Url>) {
        if !dir.exists() {
            return;
        }

        let mut builder = WalkBuilder::new(dir);
        builder.standard_filters(true);
        builder.follow_links(false);
        builder
            .filter_entry(|entry| {
                entry
                    .file_type()
                    .map(|ft| {
                        if ft.is_dir() {
                            !Self::should_skip_directory(entry.path())
                        } else {
                            true
                        }
                    })
                    .unwrap_or(true)
            })
            .types(types.clone());

        for result in builder.build() {
            let Ok(entry) = result else {
                continue;
            };

            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                continue;
            }

            if let Ok(url) = Url::from_file_path(entry.into_path()) {
                urls.insert(url);
            }
        }
    }

    fn should_skip_directory(path: &Path) -> bool {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            matches!(
                name,
                ".git"
                    | ".hg"
                    | ".svn"
                    | "node_modules"
                    | "target"
                    | ".idea"
                    | ".vscode"
                    | "__pycache__"
            )
        } else {
            false
        }
    }
}
