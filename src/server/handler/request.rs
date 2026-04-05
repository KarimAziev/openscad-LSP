use std::{
    cell::{Ref, RefCell},
    collections::HashMap,
    rc::Rc,
};

use lsp_server::{ErrorCode, RequestId, Response, ResponseError};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, CompletionParams, CompletionResponse,
    DocumentFormattingParams, DocumentSymbolParams, DocumentSymbolResponse, Documentation,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams,
    InsertTextFormat, InsertTextMode, Location, MarkupContent, Position, Range, RenameParams,
    SymbolInformation, TextDocumentPositionParams, TextEdit, Url, WorkspaceEdit,
};

use crate::{
    response_item::{Item, ItemKind},
    server::{Server, parse_code::ParsedCode},
    topiary,
    utils::*,
    workspace_index::{ResolvedSymbol, SymbolKey},
};
use tree_sitter::{Node, Point};

fn get_node_at_point<'a>(parsed_code: &'a Ref<'_, ParsedCode>, point: Point) -> Node<'a> {
    let mut cursor = parsed_code.tree.root_node().walk();
    while cursor.goto_first_child_for_point(point).is_some() {}
    cursor.node()
}

enum RenameLookup {
    NonIdentifier,
    Unresolved,
    Builtin,
    Workspace { range: Range, symbol: SymbolKey },
}

// Request handlers.
impl Server {
    fn lookup_rename_symbol(&mut self, uri: &Url, position: Position) -> Option<RenameLookup> {
        let file = self.get_code(uri)?;
        if let Ok(mut file_mut) = file.try_borrow_mut() {
            file_mut.gen_top_level_items_if_needed();
        }

        let (range, fallback_symbol) = {
            let bfile = file.borrow();
            let node = get_node_at_point(&bfile, to_point(position));
            if node.kind() != "identifier" {
                return Some(RenameLookup::NonIdentifier);
            }

            let range = Range {
                start: to_position(node.start_position()),
                end: to_position(node.end_position()),
            };
            let name = node_text(&bfile.code, &node).to_owned();
            let resolved = self.find_identities(
                &bfile,
                &|item_name| item_name == name.as_str(),
                &node,
                false,
            );
            let fallback_symbol = resolved
                .first()
                .and_then(|item| Self::resolved_symbol_from_item(&item.borrow()));

            (range, fallback_symbol)
        };

        self.ensure_workspace_index();

        let mut symbol = self.workspace_index.symbol_at(uri, &range);
        if symbol.is_none() {
            self.refresh_workspace_index_for_url(uri);
            symbol = self.workspace_index.symbol_at(uri, &range);
        }

        Some(match symbol.or(fallback_symbol) {
            Some(ResolvedSymbol::Workspace(symbol)) => RenameLookup::Workspace { range, symbol },
            Some(ResolvedSymbol::Builtin { .. }) => RenameLookup::Builtin,
            None => RenameLookup::Unresolved,
        })
    }

    fn build_rename_workspace_edit(
        &self,
        symbol: &SymbolKey,
        new_name: &str,
    ) -> Option<WorkspaceEdit> {
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for occurrence in self.workspace_index.references_for(symbol) {
            changes.entry(occurrence.url).or_default().push(TextEdit {
                range: occurrence.range,
                new_text: new_name.to_owned(),
            });
        }

        if changes.is_empty() {
            return None;
        }

        for edits in changes.values_mut() {
            edits.sort_by_key(|edit| {
                (
                    edit.range.start.line,
                    edit.range.start.character,
                    edit.range.end.line,
                    edit.range.end.character,
                )
            });
        }

        Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        })
    }

    pub(crate) fn handle_prepare_rename(
        &mut self,
        id: RequestId,
        params: TextDocumentPositionParams,
    ) {
        let uri = params.text_document.uri;
        let response = match self.lookup_rename_symbol(&uri, params.position) {
            Some(RenameLookup::Workspace { range, .. }) => Response {
                id,
                result: Some(serde_json::to_value(range).unwrap()),
                error: None,
            },
            Some(RenameLookup::Builtin) => Response {
                id,
                result: None,
                error: Some(ResponseError {
                    code: 0,
                    message: "Cannot rename builtin".to_string(),
                    data: None,
                }),
            },
            Some(RenameLookup::NonIdentifier | RenameLookup::Unresolved) => Response {
                id,
                result: None,
                error: None,
            },
            None => return,
        };
        self.respond(response)
    }

    pub(crate) fn handle_rename(&mut self, id: RequestId, params: RenameParams) {
        let uri = params.text_document_position.text_document.uri;
        let result = match self.lookup_rename_symbol(&uri, params.text_document_position.position) {
            Some(RenameLookup::Workspace { symbol, .. }) => {
                let Some(edit) = self.build_rename_workspace_edit(&symbol, &params.new_name) else {
                    self.respond(Response {
                        id,
                        result: None,
                        error: Some(ResponseError {
                            code: 0,
                            message: "No renamable references found for this symbol".to_string(),
                            data: None,
                        }),
                    });
                    return;
                };
                serde_json::to_value(edit).unwrap()
            }
            Some(RenameLookup::Builtin) => {
                self.respond(Response {
                    id,
                    result: None,
                    error: Some(ResponseError {
                        code: 0,
                        message: "Cannot rename builtin".to_string(),
                        data: None,
                    }),
                });
                return;
            }
            Some(RenameLookup::Unresolved) => {
                self.respond(Response {
                    id,
                    result: None,
                    error: Some(ResponseError {
                        code: 0,
                        message: "No definition found for this identifier".to_string(),
                        data: None,
                    }),
                });
                return;
            }
            Some(RenameLookup::NonIdentifier) => {
                self.respond(Response {
                    id,
                    result: None,
                    error: Some(ResponseError {
                        code: -32600,
                        message: "No identifier at given position".to_string(),
                        data: None,
                    }),
                });
                return;
            }
            None => return,
        };

        self.respond(Response {
            id,
            result: Some(result),
            error: None,
        });
    }
    pub(crate) fn handle_hover(&mut self, id: RequestId, params: HoverParams) {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let file = match self.get_code(uri) {
            Some(code) => code,
            _ => return,
        };

        file.borrow_mut().gen_top_level_items_if_needed();

        let point = to_point(pos);
        let bfile = file.borrow();
        let mut cursor = bfile.tree.root_node().walk();
        while cursor.goto_first_child_for_point(point).is_some() {}

        let node = cursor.node();

        let kind = node.kind();
        let name = String::from(node_text(&bfile.code, &node));

        let result = match kind {
            "identifier" => {
                let items = self.find_identities(
                    &file.borrow(),
                    &|item_name| item_name == name,
                    &node,
                    false,
                );
                items.first().map(|item| Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: lsp_types::MarkupKind::Markdown,
                        value: item.borrow_mut().get_hover(),
                    }),
                    range: None,
                })
            }
            _ => None,
        };

        let result = result.map(|r| serde_json::to_value(r).unwrap());
        self.respond(Response {
            id,
            result,
            error: None,
        });
    }

    pub(crate) fn handle_definition(&mut self, id: RequestId, params: GotoDefinitionParams) {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let file = match self.get_code(uri) {
            Some(code) => code,
            _ => return,
        };

        file.borrow_mut().gen_top_level_items_if_needed();

        let point = to_point(pos);
        let bfile = file.borrow();
        let mut cursor = bfile.tree.root_node().walk();
        while cursor.goto_first_child_for_point(point).is_some() {}

        let node = cursor.node();

        let kind = node.kind();
        let name = String::from(node_text(&bfile.code, &node));

        let result = match kind {
            "identifier" => {
                let items = self.find_identities(
                    &file.borrow(),
                    &|item_name| item_name == name,
                    &node,
                    false,
                );
                let locs = items
                    .iter()
                    .filter(|item| item.borrow().name == name && item.borrow().url.is_some())
                    .map(|item| Location {
                        uri: item.borrow().url.as_ref().unwrap().clone(),
                        range: item.borrow().range,
                    })
                    .collect::<Vec<Location>>();
                Some(locs)
            }
            "include_path" => node
                .parent()
                .and_then(|parent| bfile.get_include_url(&parent))
                .map(|url| {
                    vec![Location {
                        uri: url,
                        range: Range::default(),
                    }]
                }),
            _ => None,
        };

        let result = result.map(GotoDefinitionResponse::Array);
        let result = serde_json::to_value(result).unwrap();

        self.respond(Response {
            id,
            result: Some(result),
            error: None,
        });
    }

    pub(crate) fn handle_completion(&mut self, id: RequestId, params: CompletionParams) {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let file = match self.get_code(uri) {
            Some(code) => code,
            _ => return,
        };

        file.borrow_mut().gen_top_level_items_if_needed();

        let mut point = to_point(pos);
        let bfile = file.borrow();

        let mut node = get_node_at_point(&bfile, point);
        if node.kind() == "source_file" && point.column > 0 {
            point.column -= 1;
            node = get_node_at_point(&bfile, point);
        }

        let mut items = self.find_identities(&bfile, &|_| true, &node, true);

        let kind = node.kind();
        if let Some(parent) = &node.parent().and_then(|parent| parent.parent()) {
            let kind = parent.kind();
            let mut node = None;
            if kind == "arguments" {
                if let Some(callable) = parent.parent() {
                    let kind = callable.kind();
                    if kind == "module_call" || kind == "function_call" {
                        node = Some(callable);
                    }
                }
            }

            if kind == "module_call" || kind == "function_call" {
                node = Some(*parent);
            }

            if let Some(node) = node {
                node.child_by_field_name("name")
                    .map(|child| node_text(&bfile.code, &child))
                    .map(|name| {
                        let fun_items = self.find_identities(
                            &bfile,
                            &|item_name| item_name == name,
                            &node,
                            false,
                        );

                        if !fun_items.is_empty() {
                            let item = &fun_items[0];

                            let param_items = match &item.borrow().kind {
                                ItemKind::Module { params } => {
                                    let mut result = vec![];
                                    for p in params {
                                        result.push(Rc::new(RefCell::new(Item {
                                            name: p.name.clone(),
                                            kind: ItemKind::Variable,
                                            range: p.range,
                                            url: Some(bfile.url.clone()),
                                            ..Default::default()
                                        })));
                                    }
                                    result
                                }
                                ItemKind::Function { params } => {
                                    let mut result = vec![];
                                    for p in params {
                                        result.push(Rc::new(RefCell::new(Item {
                                            name: p.name.clone(),
                                            kind: ItemKind::Variable,
                                            range: p.range,
                                            url: Some(bfile.url.clone()),
                                            ..Default::default()
                                        })));
                                    }
                                    result
                                }
                                _ => {
                                    vec![]
                                }
                            };

                            items.extend(param_items);
                        }
                    });
            }
        }

        let builtin_url = self.builtin_url.clone();
        if !items.iter().any(|item| item.borrow().is_builtin) {
            if let Some(builtin_code) = self.get_code(&builtin_url) {
                if let Ok(mut builtin_mut) = builtin_code.try_borrow_mut() {
                    builtin_mut.gen_top_level_items_if_needed();
                }
                if let Ok(builtin_ref) = builtin_code.try_borrow() {
                    if let Some(root_items) = &builtin_ref.root_items {
                        items.extend(root_items.iter().cloned());
                    }
                }
            }
        }

        let original_items = items;
        let mut unique_items: Vec<Rc<RefCell<Item>>> = Vec::new();
        let mut key_positions: HashMap<(String, u8), usize> = HashMap::new();

        for item in original_items {
            let (key, is_builtin) = {
                let item_ref = item.borrow();
                let kind_tag = match &item_ref.kind {
                    ItemKind::Variable => 0,
                    ItemKind::Function { .. } => 1,
                    ItemKind::Keyword => 2,
                    ItemKind::Module { .. } => 3,
                };
                ((item_ref.name.clone(), kind_tag), item_ref.is_builtin)
            };

            if let Some(idx) = key_positions.get(&key) {
                let replace = {
                    let existing = unique_items[*idx].borrow();
                    existing.is_builtin && !is_builtin
                };
                if replace {
                    unique_items[*idx] = Rc::clone(&item);
                }
            } else {
                key_positions.insert(key, unique_items.len());
                unique_items.push(Rc::clone(&item));
            }
        }

        let items = unique_items;

        let include_node = if kind == "include_path" {
            Some(node)
        } else {
            let mut parent = node.parent();
            let mut include = None;
            while let Some(pnode) = parent {
                if pnode.kind().is_dependency_statement() {
                    include = pnode.child(1);
                    break;
                }
                parent = pnode.parent();
            }
            include
        };

        let result = if let Some(include_node) = include_node {
            let include_path = node_text(&bfile.code, &include_node).to_owned();
            CompletionResponse::List(CompletionList {
                is_incomplete: true,
                items: bfile
                    .get_include_completion(&include_node)
                    .iter()
                    .map(|file_name| CompletionItem {
                        label: file_name.clone(),
                        kind: Some(CompletionItemKind::FILE),
                        filter_text: Some(include_path.clone()),
                        insert_text: Some(file_name.clone()),
                        insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                        insert_text_mode: Some(InsertTextMode::ADJUST_INDENTATION),
                        ..Default::default()
                    })
                    .collect(),
            })
        } else {
            let include_defaults = self.args.include_default_params;
            CompletionResponse::List(CompletionList {
                is_incomplete: true,
                items: items
                    .iter()
                    .map(|item| {
                        let mut item_mut = item.borrow_mut();
                        let label = item_mut.name.clone();
                        let insert_text = item_mut.completion_text();
                        let completion_kind = item_mut.kind.completion_kind();
                        let filter_text = item_mut.name.clone();
                        let detail = item_mut.signature(include_defaults);
                        let hover = item_mut.get_hover();
                        drop(item_mut);

                        let documentation = if hover.trim().is_empty() {
                            None
                        } else {
                            Some(Documentation::MarkupContent(MarkupContent {
                                kind: lsp_types::MarkupKind::Markdown,
                                value: hover,
                            }))
                        };

                        CompletionItem {
                            label,
                            kind: Some(completion_kind),
                            filter_text: Some(filter_text),
                            insert_text: Some(insert_text),
                            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                            insert_text_mode: Some(InsertTextMode::ADJUST_INDENTATION),
                            detail,
                            documentation,
                            ..Default::default()
                        }
                    })
                    .collect(),
            })
        };

        let result = serde_json::to_value(result).unwrap();
        self.respond(Response {
            id,
            result: Some(result),
            error: None,
        });
    }

    pub(crate) fn handle_document_symbols(&mut self, id: RequestId, params: DocumentSymbolParams) {
        let uri = &params.text_document.uri;
        let file = match self.get_code(uri) {
            Some(code) => code,
            _ => return,
        };

        let mut bfile = file.borrow_mut();
        bfile.gen_top_level_items_if_needed();
        if let Some(items) = &bfile.root_items {
            let result: Vec<SymbolInformation> = items
                .iter()
                .filter_map(|item| {
                    item.borrow().url.as_ref().map(|url| {
                        #[allow(deprecated)]
                        SymbolInformation {
                            name: item.borrow().name.to_owned(),
                            kind: item.borrow().get_symbol_kind(),
                            tags: None,
                            deprecated: None,
                            location: Location {
                                uri: url.clone(),
                                range: item.borrow().range,
                            },
                            container_name: None,
                        }
                    })
                })
                .collect();

            let result = DocumentSymbolResponse::Flat(result);

            let result = serde_json::to_value(result).unwrap();
            self.respond(Response {
                id,
                result: Some(result),
                error: None,
            });
        }
    }

    pub(crate) fn handle_formatting(&mut self, id: RequestId, params: DocumentFormattingParams) {
        let uri = &params.text_document.uri;

        let file = match self.get_code(uri) {
            Some(code) => code,
            _ => return,
        };

        let internal_err = |err: String| {
            self.respond(Response {
                id: id.clone(),
                result: None,
                error: Some(ResponseError {
                    code: ErrorCode::InternalError as i32,
                    message: err,
                    data: None,
                }),
            });
        };

        let code = &file.borrow().code;

        let mut formatted_code: Vec<u8> = Vec::new();
        if let Err(e) = topiary::format(
            code.as_bytes(),
            &mut formatted_code,
            Some(self.args.indent.clone()),
            self.fmt_query.as_deref(),
        ) {
            internal_err(format!("topiary: {e}"));
            return;
        }
        let formatted_code = String::from_utf8(formatted_code).unwrap();
        let result = serde_json::to_value([TextEdit {
            range: file.borrow().tree.root_node().lsp_range(),
            new_text: formatted_code,
        }])
        .unwrap();

        self.respond(Response {
            id,
            result: Some(result),
            error: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cli;
    use clap::Parser;
    use lsp_server::Connection;
    use lsp_types::{
        DidChangeTextDocumentParams, DidOpenTextDocumentParams, TextDocumentContentChangeEvent,
        TextDocumentItem, VersionedTextDocumentIdentifier,
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

    fn nth_identifier_position(server: &mut Server, url: &Url, name: &str, nth: usize) -> Position {
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
                return to_position(node.start_position());
            }
            seen += 1;
        }

        panic!("identifier {name} occurrence {nth} not found");
    }

    fn rename_changes(
        server: &mut Server,
        url: &Url,
        position: Position,
        new_name: &str,
    ) -> HashMap<Url, Vec<TextEdit>> {
        let symbol = match server.lookup_rename_symbol(url, position) {
            Some(RenameLookup::Workspace { symbol, .. }) => symbol,
            Some(_) => panic!("expected workspace symbol"),
            None => panic!("lookup failed"),
        };

        server
            .build_rename_workspace_edit(&symbol, new_name)
            .expect("workspace edit")
            .changes
            .expect("changes")
    }

    #[test]
    fn rename_index_finds_references_across_files() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        let lib_path = root.join("lib.scad");
        let main_path = root.join("main.scad");

        fs::write(
            &lib_path,
            "module foo() {}\nmodule use_foo() {\n  foo();\n}\n",
        )
        .unwrap();
        fs::write(&main_path, "include <lib.scad>;\nfoo();\n").unwrap();

        let (mut server, _client_conn) = make_server(root);
        server.ensure_workspace_index();

        let lib_url = Url::from_file_path(&lib_path).unwrap();
        let main_url = Url::from_file_path(&main_path).unwrap();
        let position = nth_identifier_position(&mut server, &lib_url, "foo", 0);
        let changes = rename_changes(&mut server, &lib_url, position, "bar");

        assert_eq!(changes.get(&lib_url).map(Vec::len), Some(2));
        assert_eq!(changes.get(&main_url).map(Vec::len), Some(1));
        assert!(
            changes
                .values()
                .flatten()
                .all(|edit| edit.new_text == "bar"),
            "every edit should use the new identifier",
        );
    }

    #[test]
    fn rename_index_respects_shadowed_local_symbols() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        let file_path = root.join("local.scad");
        fs::write(
            &file_path,
            "module demo() {\n  a = 1;\n  echo(a);\n  if (true) {\n    a = 2;\n    echo(a);\n  }\n  echo(a);\n}\n",
        )
        .unwrap();

        let (mut server, _client_conn) = make_server(root);
        server.ensure_workspace_index();

        let file_url = Url::from_file_path(&file_path).unwrap();
        let position = nth_identifier_position(&mut server, &file_url, "a", 0);
        let changes = rename_changes(&mut server, &file_url, position, "outer");
        let edits = changes.get(&file_url).expect("same-file edits");
        let lines: Vec<u32> = edits.iter().map(|edit| edit.range.start.line).collect();

        assert_eq!(edits.len(), 3);
        assert_eq!(lines, vec![1, 2, 7]);
    }

    #[test]
    fn rename_index_updates_after_document_change() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        let lib_path = root.join("lib.scad");
        let main_path = root.join("main.scad");

        fs::write(&lib_path, "module foo() {}\n").unwrap();
        fs::write(&main_path, "include <lib.scad>;\nfoo();\n").unwrap();

        let (mut server, _client_conn) = make_server(root);
        server.ensure_workspace_index();

        let lib_url = Url::from_file_path(&lib_path).unwrap();
        let main_url = Url::from_file_path(&main_path).unwrap();
        let original_main = fs::read_to_string(&main_path).unwrap();

        server.handle_did_open_text_document(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: main_url.clone(),
                language_id: "openscad".to_string(),
                version: 1,
                text: original_main,
            },
        });

        server.handle_did_change_text_document(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: main_url.clone(),
                version: 2,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "include <lib.scad>;\nfoo();\nfoo();\n".to_string(),
            }],
        });

        let position = nth_identifier_position(&mut server, &lib_url, "foo", 0);
        let changes = rename_changes(&mut server, &lib_url, position, "bar");

        assert_eq!(changes.get(&main_url).map(Vec::len), Some(2));
    }

    #[test]
    fn rename_index_parameter_declaration_updates_named_arguments() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        let lib_path = root.join("lib.scad");
        let main_path = root.join("main.scad");

        fs::write(&lib_path, "module demo(width=1) { echo(width); }\n").unwrap();
        fs::write(&main_path, "include <lib.scad>;\ndemo(width=2);\n").unwrap();

        let (mut server, _client_conn) = make_server(root);
        server.ensure_workspace_index();

        let lib_url = Url::from_file_path(&lib_path).unwrap();
        let main_url = Url::from_file_path(&main_path).unwrap();
        let position = nth_identifier_position(&mut server, &lib_url, "width", 0);
        let changes = rename_changes(&mut server, &lib_url, position, "size");

        assert_eq!(changes.get(&lib_url).map(Vec::len), Some(2));
        assert_eq!(changes.get(&main_url).map(Vec::len), Some(1));
        assert!(
            changes
                .values()
                .flatten()
                .all(|edit| edit.new_text == "size")
        );
    }

    #[test]
    fn rename_index_named_argument_resolves_back_to_parameter_declaration() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        let lib_path = root.join("lib.scad");
        let main_path = root.join("main.scad");

        fs::write(&lib_path, "module demo(width=1) { echo(width); }\n").unwrap();
        fs::write(&main_path, "include <lib.scad>;\ndemo(width=2);\n").unwrap();

        let (mut server, _client_conn) = make_server(root);
        server.ensure_workspace_index();

        let lib_url = Url::from_file_path(&lib_path).unwrap();
        let main_url = Url::from_file_path(&main_path).unwrap();
        let position = nth_identifier_position(&mut server, &main_url, "width", 0);
        let changes = rename_changes(&mut server, &main_url, position, "size");

        assert_eq!(changes.get(&lib_url).map(Vec::len), Some(2));
        assert_eq!(changes.get(&main_url).map(Vec::len), Some(1));
        assert!(
            changes
                .values()
                .flatten()
                .all(|edit| edit.new_text == "size")
        );
    }

    #[test]
    fn rename_index_use_does_not_export_globals() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        let lib_path = root.join("lib.scad");
        let main_path = root.join("main.scad");

        fs::write(&lib_path, "x = 1;\nmodule foo() { echo(x); }\n").unwrap();
        fs::write(&main_path, "use <lib.scad>;\nfoo();\necho(x);\n").unwrap();

        let (mut server, _client_conn) = make_server(root);
        server.ensure_workspace_index();

        let lib_url = Url::from_file_path(&lib_path).unwrap();
        let main_url = Url::from_file_path(&main_path).unwrap();

        let foo_position = nth_identifier_position(&mut server, &lib_url, "foo", 0);
        let foo_changes = rename_changes(&mut server, &lib_url, foo_position, "bar");
        assert_eq!(foo_changes.get(&main_url).map(Vec::len), Some(1));

        let x_position = nth_identifier_position(&mut server, &lib_url, "x", 0);
        let x_changes = rename_changes(&mut server, &lib_url, x_position, "y");
        assert_eq!(x_changes.get(&lib_url).map(Vec::len), Some(2));
        assert!(
            !x_changes.contains_key(&main_url),
            "globals from a used file should not be visible in the using file",
        );
    }

    #[test]
    fn rename_index_nested_use_is_not_reexported() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        let nested_path = root.join("nested.scad");
        let mid_path = root.join("mid.scad");
        let main_path = root.join("main.scad");

        fs::write(&nested_path, "module inner() {}\n").unwrap();
        fs::write(
            &mid_path,
            "use <nested.scad>;\nmodule outer() { inner(); }\n",
        )
        .unwrap();
        fs::write(&main_path, "use <mid.scad>;\nouter();\ninner();\n").unwrap();

        let (mut server, _client_conn) = make_server(root);
        server.ensure_workspace_index();

        let nested_url = Url::from_file_path(&nested_path).unwrap();
        let mid_url = Url::from_file_path(&mid_path).unwrap();
        let main_url = Url::from_file_path(&main_path).unwrap();

        let outer_position = nth_identifier_position(&mut server, &mid_url, "outer", 0);
        let outer_changes = rename_changes(&mut server, &mid_url, outer_position, "wrapper");
        assert_eq!(outer_changes.get(&main_url).map(Vec::len), Some(1));

        let inner_position = nth_identifier_position(&mut server, &nested_url, "inner", 0);
        let inner_changes = rename_changes(&mut server, &nested_url, inner_position, "helper");
        assert_eq!(inner_changes.get(&mid_url).map(Vec::len), Some(1));
        assert!(
            !inner_changes.contains_key(&main_url),
            "nested use should not export its callables to the base file",
        );
    }

    #[test]
    fn rename_index_include_reexports_nested_use_callables() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        let nested_path = root.join("nested.scad");
        let mid_path = root.join("mid.scad");
        let main_path = root.join("main.scad");

        fs::write(&nested_path, "module inner() {}\n").unwrap();
        fs::write(
            &mid_path,
            "use <nested.scad>;\nmodule outer() { inner(); }\n",
        )
        .unwrap();
        fs::write(&main_path, "include <mid.scad>;\nouter();\ninner();\n").unwrap();

        let (mut server, _client_conn) = make_server(root);
        server.ensure_workspace_index();

        let nested_url = Url::from_file_path(&nested_path).unwrap();
        let main_url = Url::from_file_path(&main_path).unwrap();

        let inner_position = nth_identifier_position(&mut server, &nested_url, "inner", 0);
        let inner_changes = rename_changes(&mut server, &nested_url, inner_position, "helper");
        assert_eq!(inner_changes.get(&main_url).map(Vec::len), Some(1));
    }
}
