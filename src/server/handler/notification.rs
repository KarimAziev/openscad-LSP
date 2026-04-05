use std::{collections::HashSet, env, path::PathBuf};

use lsp_types::{
    Diagnostic, DiagnosticSeverity, DidChangeConfigurationParams, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidChangeWatchedFilesRegistrationOptions,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    FileChangeType, FileSystemWatcher, GlobPattern, InitializedParams, OneOf, Pattern,
    PublishDiagnosticsParams, Registration, RegistrationParams, RelativePattern, Url, WatchKind,
};
use lsp_types::{
    notification::{DidChangeWatchedFiles, Notification as LspNotification},
    request::RegisterCapability,
};
use serde::Deserialize;

use crate::{server::Server, utils::*};

// Notification handlers.
impl Server {
    pub(crate) fn handle_initialized(&mut self, _params: InitializedParams) {
        self.register_workspace_file_watchers();
    }

    pub(crate) fn handle_did_open_text_document(&mut self, params: DidOpenTextDocumentParams) {
        let DidOpenTextDocumentParams { text_document: doc } = params;
        self.open_documents.insert(doc.uri.clone());
        self.insert_code(doc.uri.clone(), doc.text);
        self.refresh_workspace_index_for_url(&doc.uri);
    }

    pub(crate) fn handle_did_change_text_document(&mut self, params: DidChangeTextDocumentParams) {
        let DidChangeTextDocumentParams {
            text_document,
            content_changes,
        } = params;
        let uri = text_document.uri.clone();

        let pc = match self.codes.get_refresh(&uri) {
            Some(x) => x,
            None => {
                err_to_console!("unknown document {}", uri);
                return;
            }
        };

        pc.borrow_mut().edit(&content_changes);

        let mut diags: Vec<_> = error_nodes(pc.borrow().tree.walk())
            .into_iter()
            .map(|node| Diagnostic {
                range: node.lsp_range(),
                severity: Some(DiagnosticSeverity::ERROR),
                message: if node.is_missing() {
                    format!("missing {}", node.kind())
                } else {
                    "syntax error".to_owned()
                },
                ..Default::default()
            })
            .collect();

        if content_changes.len() == 1 {
            if let Some(range) = content_changes[0].range {
                let bpc = pc.borrow();
                let pos = to_point(range.start);
                let mut cursor = bpc.tree.root_node().walk();
                cursor.goto_first_child_for_point(pos);
                let node = cursor.node();
                let kind = node.kind();
                // let text = node_text(&bpc.code, &node);

                if kind.is_dependency_statement() && bpc.get_include_url(&node).is_none() {
                    let mut range = node.child(1).unwrap().lsp_range();
                    range.start.character += 1;
                    range.end.character -= 1;
                    diags.push(Diagnostic {
                        range,
                        severity: Some(DiagnosticSeverity::ERROR),
                        message: "file not found!".to_owned(),
                        ..Default::default()
                    });
                }
            }
        }

        self.notify(lsp_server::Notification::new(
            "textDocument/publishDiagnostics".into(),
            PublishDiagnosticsParams {
                uri: uri.clone(),
                diagnostics: diags,
                version: Some(text_document.version),
            },
        ));

        self.refresh_workspace_index_for_url(&uri);
    }

    pub(crate) fn handle_did_change_watched_files(&mut self, params: DidChangeWatchedFilesParams) {
        let mut changed_urls = HashSet::new();
        let mut needs_rebuild = false;

        for change in params.changes {
            if !self.is_watched_scad_file(&change.uri) || self.open_documents.contains(&change.uri)
            {
                continue;
            }

            match change.typ {
                typ if typ == FileChangeType::CHANGED => {
                    let is_known = self.workspace_index.files.contains_key(&change.uri)
                        || self.codes.contains_key(&change.uri);
                    if is_known {
                        if self.read_and_cache(change.uri.clone()).is_ok() {
                            changed_urls.insert(change.uri);
                        } else {
                            self.codes.remove(&change.uri);
                            needs_rebuild = true;
                        }
                    } else {
                        self.codes.remove(&change.uri);
                        let _ = self.read_and_cache(change.uri.clone());
                        needs_rebuild = true;
                    }
                }
                typ if typ == FileChangeType::CREATED => {
                    self.codes.remove(&change.uri);
                    let _ = self.read_and_cache(change.uri.clone());
                    needs_rebuild = true;
                }
                typ if typ == FileChangeType::DELETED => {
                    self.codes.remove(&change.uri);
                    needs_rebuild = true;
                }
                _ => {}
            }
        }

        if needs_rebuild {
            // File creation/deletion can change whether previously missing dependencies resolve, so
            // rebuild the workspace graph instead of trusting reverse edges from the old state.
            self.rebuild_workspace_index();
            return;
        }

        for url in changed_urls {
            self.refresh_workspace_index_for_url(&url);
        }
    }

    pub(crate) fn handle_did_change_config(&mut self, params: DidChangeConfigurationParams) {
        #[derive(Deserialize)]
        pub(crate) struct Openscad {
            search_paths: Option<String>,
            default_param: Option<bool>,
            indent: Option<String>,
            query_file: Option<PathBuf>,
        }

        #[derive(Deserialize)]
        pub(crate) struct Settings {
            openscad: Openscad,
        }

        let settings = match serde_json::from_value::<Settings>(params.settings) {
            Ok(settings) => Some(settings),
            Err(err) => {
                err_to_console!("{}", err.to_string());
                return;
            }
        };

        if let Some(settings) = settings {
            // self.extend_libs(settings.search_paths);
            let paths: Vec<String> = settings
                .openscad
                .search_paths
                .map(|paths| {
                    env::split_paths(&paths)
                        .filter_map(|buf| buf.into_os_string().into_string().ok())
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default();

            self.extend_libs(paths);

            if let Some(default_param) = settings.openscad.default_param {
                self.args.include_default_params = default_param;
            }

            self.args.indent = match settings.openscad.indent {
                Some(indent) => {
                    if indent.is_empty() {
                        "  ".to_owned()
                    } else {
                        indent
                    }
                }
                None => "  ".to_owned(),
            };
            self.fmt_query = Self::get_fmt_query(settings.openscad.query_file);
        }

        for code in self.codes.values() {
            code.borrow_mut().changed = true;
        }
        self.clear_workspace_index();
    }

    pub(crate) fn handle_did_save_text_document(&mut self, _params: DidSaveTextDocumentParams) {}

    pub(crate) fn handle_did_close_text_document(&mut self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.open_documents.remove(&uri);

        if self.read_and_cache(uri.clone()).is_err() {
            self.codes.remove(&uri);
        }

        self.refresh_workspace_index_for_url(&uri);
    }

    fn register_workspace_file_watchers(&mut self) {
        if !self.supports_dynamic_watched_files_registration()
            || self.watched_files_registered()
            || self.watched_files_registration_pending()
        {
            return;
        }

        let watchers = self.workspace_file_watchers();
        if watchers.is_empty() {
            return;
        }

        let request_id = self.send_request::<RegisterCapability>(RegistrationParams {
            registrations: vec![Registration {
                id: "openscad-lsp-watch-scad".to_owned(),
                method: DidChangeWatchedFiles::METHOD.to_owned(),
                register_options: Some(
                    serde_json::to_value(DidChangeWatchedFilesRegistrationOptions { watchers })
                        .expect("serialize watched-files registration"),
                ),
            }],
        });
        self.mark_watched_files_registration_pending(request_id);
    }

    fn workspace_file_watchers(&self) -> Vec<FileSystemWatcher> {
        let kind = Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete);
        if self.supports_relative_watch_patterns() && !self.workspace_roots.is_empty() {
            return self
                .workspace_roots
                .iter()
                .filter_map(|root| Url::from_directory_path(root).ok())
                .map(|base_uri| FileSystemWatcher {
                    glob_pattern: GlobPattern::Relative(RelativePattern {
                        base_uri: OneOf::Right(base_uri),
                        pattern: Pattern::from("**/*.scad"),
                    }),
                    kind,
                })
                .collect();
        }

        vec![FileSystemWatcher {
            glob_pattern: GlobPattern::String(Pattern::from("**/*.scad")),
            kind,
        }]
    }

    fn is_watched_scad_file(&self, uri: &Url) -> bool {
        if *uri == self.builtin_url {
            return false;
        }

        uri.to_file_path()
            .ok()
            .and_then(|path| {
                path.extension()
                    .and_then(|extension| extension.to_str().map(str::to_owned))
            })
            .is_some_and(|extension| extension.eq_ignore_ascii_case("scad"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, server::workspace_index::ResolvedSymbol};
    use clap::Parser;
    use lsp_server::{Connection, Message};
    use lsp_types::{
        ClientCapabilities, DidChangeWatchedFilesClientCapabilities, FileEvent, InitializeParams,
        Range, WorkspaceClientCapabilities,
        request::{RegisterCapability, Request as LspRequest},
    };
    use std::{fs, path::Path};
    use tempfile::tempdir;
    use tree_sitter_traversal2::{Order, traverse};

    fn make_server(workspace_root: &Path) -> (Server, Connection) {
        let (server_conn, client_conn) = Connection::memory();
        let args = Cli::parse_from(["openscad-lsp"]);
        let mut server = Server::new(server_conn, args);
        server.workspace_roots = vec![workspace_root.to_path_buf()];
        (server, client_conn)
    }

    fn nth_identifier_range(server: &mut Server, url: &Url, name: &str, nth: usize) -> Range {
        let code = server.get_code(url).expect("load file");
        if let Ok(mut code_mut) = code.try_borrow_mut() {
            code_mut.gen_top_level_items_if_needed();
        }
        let code_ref = code.borrow();
        let cursor = code_ref.tree.walk();
        let mut seen = 0;

        for node in traverse(cursor, Order::Pre) {
            if node.kind() != "identifier" || node_text(&code_ref.code, &node) != name {
                continue;
            }

            if seen == nth {
                return Range {
                    start: to_position(node.start_position()),
                    end: to_position(node.end_position()),
                };
            }
            seen += 1;
        }

        panic!("identifier {name} occurrence {nth} not found");
    }

    #[test]
    fn initialized_registers_workspace_watchers() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let (mut server, client_conn) = make_server(root);

        server.configure_client_capabilities(&InitializeParams {
            capabilities: ClientCapabilities {
                workspace: Some(WorkspaceClientCapabilities {
                    did_change_watched_files: Some(DidChangeWatchedFilesClientCapabilities {
                        dynamic_registration: Some(true),
                        relative_pattern_support: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        });

        server.handle_initialized(InitializedParams {});

        let message = client_conn.receiver.recv().expect("registration request");
        let Message::Request(request) = message else {
            panic!("expected request, got {message:?}");
        };
        assert_eq!(request.method, RegisterCapability::METHOD);

        let params: RegistrationParams = serde_json::from_value(request.params).unwrap();
        assert_eq!(params.registrations.len(), 1);
        assert_eq!(
            params.registrations[0].method,
            DidChangeWatchedFiles::METHOD,
        );

        let options: DidChangeWatchedFilesRegistrationOptions =
            serde_json::from_value(params.registrations[0].register_options.clone().unwrap())
                .unwrap();
        assert_eq!(options.watchers.len(), 1);
        match &options.watchers[0].glob_pattern {
            GlobPattern::Relative(pattern) => assert_eq!(pattern.pattern, "**/*.scad"),
            glob_pattern => panic!("expected relative glob pattern, got {glob_pattern:?}"),
        }
    }

    #[test]
    fn watched_file_change_refreshes_closed_document_index() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        let lib_path = root.join("lib.scad");
        let main_path = root.join("main.scad");

        fs::write(&lib_path, "module foo() {}\n").unwrap();
        fs::write(&main_path, "include <lib.scad>;\nfoo();\n").unwrap();

        let (mut server, _client_conn) = make_server(root);
        server.ensure_workspace_index();

        fs::write(&main_path, "include <lib.scad>;\nfoo();\nfoo();\n").unwrap();
        let main_url = Url::from_file_path(&main_path).unwrap();
        server.handle_did_change_watched_files(DidChangeWatchedFilesParams {
            changes: vec![FileEvent::new(main_url.clone(), FileChangeType::CHANGED)],
        });

        let lib_url = Url::from_file_path(&lib_path).unwrap();
        let foo_range = nth_identifier_range(&mut server, &lib_url, "foo", 0);
        let symbol = match server.workspace_index.symbol_at(&lib_url, &foo_range) {
            Some(ResolvedSymbol::Workspace(symbol)) => symbol,
            result => panic!("expected workspace symbol, got {result:?}"),
        };

        let refs = server.workspace_index.references_for(&symbol);
        let main_refs = refs
            .iter()
            .filter(|occurrence| occurrence.url == main_url)
            .count();
        assert_eq!(main_refs, 2);
    }

    #[test]
    fn watched_file_create_rebuilds_missing_dependency_edges() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        let lib_path = root.join("lib.scad");
        let main_path = root.join("main.scad");

        fs::write(&main_path, "include <lib.scad>;\nfoo();\n").unwrap();

        let (mut server, _client_conn) = make_server(root);
        server.ensure_workspace_index();

        fs::write(&lib_path, "module foo() {}\n").unwrap();
        let lib_url = Url::from_file_path(&lib_path).unwrap();
        server.handle_did_change_watched_files(DidChangeWatchedFilesParams {
            changes: vec![FileEvent::new(lib_url.clone(), FileChangeType::CREATED)],
        });

        let foo_range = nth_identifier_range(&mut server, &lib_url, "foo", 0);
        let symbol = match server.workspace_index.symbol_at(&lib_url, &foo_range) {
            Some(ResolvedSymbol::Workspace(symbol)) => symbol,
            result => panic!("expected workspace symbol, got {result:?}"),
        };

        let main_url = Url::from_file_path(&main_path).unwrap();
        let refs = server.workspace_index.references_for(&symbol);
        let main_refs = refs
            .iter()
            .filter(|occurrence| occurrence.url == main_url)
            .count();
        assert_eq!(main_refs, 1);
    }

    #[test]
    fn watched_file_delete_removes_resolved_symbol_links() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        let lib_path = root.join("lib.scad");
        let main_path = root.join("main.scad");

        fs::write(&lib_path, "module foo() {}\n").unwrap();
        fs::write(&main_path, "include <lib.scad>;\nfoo();\n").unwrap();

        let (mut server, _client_conn) = make_server(root);
        server.ensure_workspace_index();

        fs::remove_file(&lib_path).unwrap();
        let lib_url = Url::from_file_path(&lib_path).unwrap();
        server.handle_did_change_watched_files(DidChangeWatchedFilesParams {
            changes: vec![FileEvent::new(lib_url, FileChangeType::DELETED)],
        });

        let main_url = Url::from_file_path(&main_path).unwrap();
        let foo_range = nth_identifier_range(&mut server, &main_url, "foo", 0);
        assert!(
            server
                .workspace_index
                .symbol_at(&main_url, &foo_range)
                .is_none(),
            "deleted includes should no longer leave stale symbol links in dependents",
        );
    }
}
