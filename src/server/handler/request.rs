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
    InsertTextFormat, InsertTextMode, Location, MarkupContent, Range, RenameParams,
    SymbolInformation, TextDocumentPositionParams, TextEdit, Url, WorkspaceEdit,
};

use tree_sitter::{Node, Point};
use tree_sitter_traversal2::{Order, traverse};

use crate::{
    response_item::{Item, ItemKind},
    server::{Server, parse_code::ParsedCode},
    topiary,
    utils::*,
};

fn get_node_at_point<'a>(parsed_code: &'a Ref<'_, ParsedCode>, point: Point) -> Node<'a> {
    let mut cursor = parsed_code.tree.root_node().walk();
    while cursor.goto_first_child_for_point(point).is_some() {}
    cursor.node()
}

// Request handlers.
impl Server {
    pub(crate) fn handle_prepare_rename(
        &mut self,
        id: RequestId,
        params: TextDocumentPositionParams,
    ) {
        let uri = params.text_document.uri;

        let file = match self.get_code(&uri) {
            Some(code) => code,
            _ => return,
        };
        file.borrow_mut().gen_top_level_items_if_needed();
        let bfile = file.borrow();

        let node = get_node_at_point(&bfile, to_point(params.position));
        if node.kind() != "identifier" {
            self.respond(Response {
                id,
                result: None,
                error: None,
            });
            return;
        }
        let ident_name = node_text(&bfile.code, &node);
        let identifier_definition =
            self.find_identities(&bfile, &|name| name == ident_name, &node, false);

        let definition = if let Some(def) = identifier_definition.first() {
            def
        } else {
            self.respond(Response {
                id,
                result: None,
                error: None,
            });
            return;
        };

        if definition.borrow().url.is_none() {
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

        self.respond(Response {
            id,
            result: Some(
                serde_json::to_value(Range {
                    start: to_position(node.start_position()),
                    end: to_position(node.end_position()),
                })
                .unwrap(),
            ),
            error: None,
        })
    }
    pub(crate) fn handle_rename(&mut self, id: RequestId, params: RenameParams) {
        let uri = params.text_document_position.text_document.uri;
        let ident_new_name = params.new_name;

        let file = match self.get_code(&uri) {
            Some(code) => code,
            _ => return,
        };
        if let Ok(mut file_mut) = file.try_borrow_mut() {
            file_mut.gen_top_level_items_if_needed();
        }

        let (ident_initial_name, identifier_definition) = {
            let bfile = file.borrow();
            let node = get_node_at_point(&bfile, to_point(params.text_document_position.position));
            if node.kind() != "identifier" {
                self.respond(Response {
                    id,
                    result: None,
                    error: Some(ResponseError {
                        code: -32600, // Invalid Request error
                        message: "No identifier at given position".to_string(),
                        data: None,
                    }),
                });
                return;
            }
            let ident_initial_name = node_text(&bfile.code, &node).to_string();
            let identifier_definition = self.find_identities(
                &bfile,
                &|name| name == ident_initial_name.as_str(),
                &node,
                false,
            );
            (ident_initial_name, identifier_definition)
        };

        let definition = if let Some(def) = identifier_definition.first() {
            def
        } else {
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
        };

        let (definition_url_opt, definition_range, is_global_symbol) = {
            let def = definition.borrow();
            let is_global_symbol = match (&def.kind, def.is_top_level) {
                (ItemKind::Function { .. }, _) | (ItemKind::Module { .. }, _) => true,
                (ItemKind::Variable, true) => true,
                _ => false,
            };
            (def.url.clone(), def.range.clone(), is_global_symbol)
        };

        let definition_url = if let Some(url) = definition_url_opt {
            url
        } else {
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
        };

        let def_code = match self.get_code(&definition_url) {
            Some(code) => code,
            _ => {
                self.respond(Response {
                    id,
                    result: None,
                    error: Some(ResponseError {
                        code: 0,
                        message: "Definition file is not available".to_string(),
                        data: None,
                    }),
                });
                return;
            }
        };
        if let Ok(mut code_mut) = def_code.try_borrow_mut() {
            code_mut.gen_top_level_items_if_needed();
        }

        if !is_global_symbol {
            let changes = {
                let def_file = def_code.borrow();
                let definition_node =
                    get_node_at_point(&def_file, to_point(definition_range.start));
                // unwrap here is fine because an identifier node should always have a parent scope
                let parent_scope = find_node_scope(definition_node).unwrap();
                let ident_initial_node = definition_node;

                let mut node_iter = traverse(parent_scope.walk(), Order::Post);
                let mut edits = vec![];
                while let Some(node) = node_iter.next() {
                    let is_identifier_instance = node.kind() != "identifier"
                        || node_text(&def_file.code, &node) != ident_initial_name.as_str();
                    if is_identifier_instance {
                        continue;
                    }

                    let is_assignment = node
                        .parent()
                        .is_some_and(|node| node.kind() == "assignment");
                    let is_assignment_in_subscope = is_assignment && node != ident_initial_node;
                    if is_assignment_in_subscope {
                        // Unwrap is ok because an identifier node would always have a parent scope.
                        let scope = find_node_scope(node).unwrap();
                        // Consume iterator until it reaches the parent scope
                        while node_iter.next().is_some_and(|next| scope != next) {}
                        continue;
                    }

                    edits.push(TextEdit {
                        range: Range {
                            start: to_position(node.start_position()),
                            end: to_position(node.end_position()),
                        },
                        new_text: ident_new_name.clone(),
                    });
                }
                edits
            };

            if changes.is_empty() {
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
            }

            let mut changes_map = HashMap::new();
            changes_map.insert(definition_url, changes);

            let result = WorkspaceEdit {
                changes: Some(changes_map),
                ..Default::default()
            };

            self.respond(Response {
                id,
                result: Some(serde_json::to_value(result).unwrap()),
                error: None,
            });
            return;
        }

        let base_seeds = vec![uri.clone(), definition_url.clone()];
        let related_urls: Vec<Url> = {
            let mut urls = self.collect_related_urls(&base_seeds);
            urls.insert(definition_url.clone());
            urls.insert(uri.clone());
            urls.into_iter().collect()
        };

        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();

        for url in related_urls {
            let code = match self.get_code(&url) {
                Some(code) => code,
                None => continue,
            };
            if let Ok(mut code_mut) = code.try_borrow_mut() {
                code_mut.gen_top_level_items_if_needed();
            }
            let code_ref = code.borrow();
            let cursor = code_ref.tree.walk();
            let mut iter = traverse(cursor, Order::Pre);
            let mut edits = vec![];

            while let Some(node) = iter.next() {
                if node.kind() != "identifier" {
                    continue;
                }
                if node_text(&code_ref.code, &node) != ident_initial_name.as_str() {
                    continue;
                }

                let resolved = self.find_identities(
                    &code_ref,
                    &|name| name == ident_initial_name.as_str(),
                    &node,
                    false,
                );
                if let Some(item) = resolved.first() {
                    let item_ref = item.borrow();
                    if item_ref.url.as_ref() == Some(&definition_url)
                        && item_ref.range == definition_range
                    {
                        edits.push(TextEdit {
                            range: Range {
                                start: to_position(node.start_position()),
                                end: to_position(node.end_position()),
                            },
                            new_text: ident_new_name.clone(),
                        });
                    }
                }
            }

            if !edits.is_empty() {
                changes.insert(url, edits);
            }
        }

        if changes.is_empty() {
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
        }

        let result = WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        };

        self.respond(Response {
            id,
            result: Some(serde_json::to_value(result).unwrap()),
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
            "include_path" => {
                let mut res = None;
                if let Some(incs) = &(file.borrow().includes) {
                    let include_path = name
                        .trim_start_matches(&['<', '\n'][..])
                        .trim_end_matches(&['>', '\n'][..]);

                    let mut inciter = incs.iter();
                    let loc = loop {
                        if let Some(url) = inciter.next() {
                            if url.path().ends_with(include_path) {
                                break Some(Location {
                                    uri: url.clone(),
                                    range: Range::default(),
                                });
                            }
                        } else {
                            break None;
                        }
                    };

                    if let Some(v) = loc {
                        res = Some(vec![v]);
                    }
                };
                res
            }
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

        if point.column > 0 {
            point.column -= 1;
        } else {
            point.row -= 1;
        }

        let bfile = file.borrow();
        let mut cursor = bfile.tree.root_node().walk();

        while cursor.goto_first_child_for_point(point).is_some() {}

        let node = cursor.node();
        let name = node_text(&bfile.code, &node);

        let mut items = self.find_identities(&*bfile, &|_| true, &node, true);

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
                            &*bfile,
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

        let result = if kind == "include_path"
            || node
                .prev_sibling()
                .map(|sib| {
                    if sib.kind() == "include" || sib.kind() == "use" {
                        Some(true)
                    } else {
                        None
                    }
                })
                .is_some()
        {
            CompletionResponse::List(CompletionList {
                is_incomplete: true,
                items: bfile
                    .get_include_completion(&node)
                    .iter()
                    .map(|file_name| CompletionItem {
                        label: file_name.clone(),
                        kind: Some(CompletionItemKind::FILE),
                        filter_text: Some(name.to_owned()),
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
