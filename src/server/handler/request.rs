use std::{
    cell::{Ref, RefCell},
    collections::{HashMap, HashSet},
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

struct ArgumentNameCompletionContext<'a> {
    call: Node<'a>,
    prefix: String,
    positional_count: usize,
    used_named_args: HashSet<String>,
}

// Request handlers.
impl Server {
    fn node_contains_byte(node: &Node, byte_offset: usize) -> bool {
        node.start_byte() <= byte_offset && byte_offset <= node.end_byte()
    }

    fn argument_call_context<'a>(
        node: Node<'a>,
        byte_offset: usize,
    ) -> Option<(Node<'a>, Node<'a>)> {
        let mut current = Some(node);
        while let Some(current_node) = current {
            if current_node.kind() == "arguments" {
                let call = current_node.parent()?;
                if call.kind() == "module_call" || call.kind() == "function_call" {
                    return Some((call, current_node));
                }
            }

            if current_node.kind() == "module_call" || current_node.kind() == "function_call" {
                if let Some(arguments) = current_node.child_by_field_name("arguments") {
                    if Self::node_contains_byte(&arguments, byte_offset) {
                        return Some((current_node, arguments));
                    }
                }
            }

            current = current_node.parent();
        }

        None
    }

    fn direct_argument_at<'a>(arguments: Node<'a>, byte_offset: usize) -> Option<Node<'a>> {
        arguments
            .named_children(&mut arguments.walk())
            .find(|child| Self::node_contains_byte(child, byte_offset))
    }

    fn argument_assignment_ancestor<'a>(node: Node<'a>, arguments: Node<'a>) -> Option<Node<'a>> {
        let mut current = Some(node);
        while let Some(current_node) = current {
            if current_node == arguments {
                return None;
            }

            if current_node.kind() == "assignment"
                && current_node
                    .parent()
                    .is_some_and(|parent| parent == arguments)
            {
                return Some(current_node);
            }

            current = current_node.parent();
        }

        None
    }

    fn is_argument_value_context(
        code: &str,
        node: Node,
        arguments: Node,
        byte_offset: usize,
    ) -> bool {
        if let Some(assignment) = Self::argument_assignment_ancestor(node, arguments) {
            if let Some(value) = assignment.child_by_field_name("value") {
                if byte_offset >= value.start_byte() {
                    return true;
                }
            }

            let Some(name) = assignment.child_by_field_name("name") else {
                return false;
            };

            if byte_offset <= name.end_byte() {
                return false;
            }

            let end = byte_offset.min(assignment.end_byte()).min(code.len());
            return code
                .get(name.end_byte()..end)
                .is_some_and(|between_name_and_cursor| between_name_and_cursor.contains('='));
        }

        let start = code[..byte_offset.min(code.len())]
            .rfind([',', '('])
            .map_or(arguments.start_byte(), |index| index + 1)
            .max(arguments.start_byte());
        code.get(start..byte_offset.min(code.len()))
            .is_some_and(|current_argument_text| current_argument_text.contains('='))
    }

    fn argument_name_prefix(code: &str, node: Node, arguments: Node, byte_offset: usize) -> String {
        let Some(argument) = Self::direct_argument_at(arguments, byte_offset) else {
            let start = code[..byte_offset.min(code.len())]
                .rfind(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'))
                .map_or(arguments.start_byte(), |index| index + 1)
                .max(arguments.start_byte());
            return code
                .get(start..byte_offset.min(code.len()))
                .unwrap_or_default()
                .to_owned();
        };

        if argument.kind() == "identifier" && Self::node_contains_byte(&argument, byte_offset) {
            let end = byte_offset.min(argument.end_byte()).min(code.len());
            return code
                .get(argument.start_byte()..end)
                .unwrap_or_default()
                .to_owned();
        }

        let mut current = Some(node);
        while let Some(current_node) = current {
            if current_node == arguments {
                break;
            }

            if current_node.kind() == "identifier"
                && Self::node_contains_byte(&current_node, byte_offset)
                && Self::node_contains_byte(&argument, byte_offset)
            {
                let end = byte_offset.min(current_node.end_byte()).min(code.len());
                return code
                    .get(current_node.start_byte()..end)
                    .unwrap_or_default()
                    .to_owned();
            }

            current = current_node.parent();
        }

        String::new()
    }

    fn argument_name_completion_context<'a>(
        code: &str,
        node: Node<'a>,
        byte_offset: usize,
    ) -> Option<ArgumentNameCompletionContext<'a>> {
        let (call, arguments) = Self::argument_call_context(node, byte_offset)?;

        if Self::is_argument_value_context(code, node, arguments, byte_offset) {
            return None;
        }

        if let Some(argument) = Self::direct_argument_at(arguments, byte_offset) {
            if argument.kind() != "assignment"
                && argument.kind() != "identifier"
                && argument.start_byte() < byte_offset
            {
                return None;
            }
        }

        let current_argument_start = Self::direct_argument_at(arguments, byte_offset)
            .map_or(byte_offset, |argument| argument.start_byte());
        let current_assignment = Self::argument_assignment_ancestor(node, arguments);
        let mut positional_count = 0;
        let mut used_named_args = HashSet::new();

        for argument in arguments.named_children(&mut arguments.walk()) {
            if current_assignment.is_some_and(|assignment| assignment == argument) {
                continue;
            }

            if argument.kind() == "assignment" {
                if let Some(name) = argument.child_by_field_name("name") {
                    used_named_args.insert(node_text(code, &name).to_owned());
                }
            } else if argument.end_byte() <= current_argument_start {
                positional_count += 1;
            }
        }

        Some(ArgumentNameCompletionContext {
            call,
            prefix: Self::argument_name_prefix(code, node, arguments, byte_offset),
            positional_count,
            used_named_args,
        })
    }

    fn argument_name_completion_items(
        &mut self,
        code: &ParsedCode,
        context: &ArgumentNameCompletionContext<'_>,
    ) -> Vec<Rc<RefCell<Item>>> {
        let Some(call_name_node) = context.call.child_by_field_name("name") else {
            return vec![];
        };
        let call_name = node_text(&code.code, &call_name_node).to_owned();
        let callable_items = self.find_identities(
            code,
            &|item_name| item_name == call_name.as_str(),
            &context.call,
            true,
        );

        let mut items = vec![];
        let mut seen = HashSet::new();
        for item in callable_items {
            let (params, url, is_builtin) = {
                let item_ref = item.borrow();
                let params = match &item_ref.kind {
                    ItemKind::Module { params } | ItemKind::Function { params } => params.clone(),
                    _ => continue,
                };
                (params, item_ref.url.clone(), item_ref.is_builtin)
            };

            for param in params
                .into_iter()
                .skip(context.positional_count)
                .filter(|param| param.name.starts_with(&context.prefix))
                .filter(|param| !context.used_named_args.contains(&param.name))
            {
                if !seen.insert(param.name.clone()) {
                    continue;
                }

                items.push(Rc::new(RefCell::new(Item {
                    name: param.name,
                    kind: ItemKind::Variable,
                    range: param.range,
                    url: url.clone(),
                    is_builtin,
                    ..Default::default()
                })));
            }
        }

        items
    }

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

        let byte_offset = find_offset(&bfile.code, pos).unwrap_or(bfile.code.len());
        let argument_name_context =
            Self::argument_name_completion_context(&bfile.code, node, byte_offset);
        let mut items = match &argument_name_context {
            Some(context) => self.argument_name_completion_items(&bfile, context),
            None => self.find_identities(&bfile, &|_| true, &node, true),
        };

        let builtin_url = self.builtin_url.clone();
        if argument_name_context.is_none() && !items.iter().any(|item| item.borrow().is_builtin) {
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
        let kind = node.kind();

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
    use lsp_server::{Connection, Message};
    use lsp_types::{
        DidChangeTextDocumentParams, DidOpenTextDocumentParams, PartialResultParams,
        TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
        VersionedTextDocumentIdentifier, WorkDoneProgressParams,
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

    fn source_and_position(source: &str) -> (String, Position) {
        let marker_start = source.find('$').expect("cursor marker");
        let marker_len = if source[marker_start..].starts_with("$0") {
            2
        } else {
            1
        };
        let before_marker = &source[..marker_start];
        let line = before_marker.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let character = before_marker
            .rsplit_once('\n')
            .map_or(before_marker, |(_, line)| line)
            .chars()
            .count() as u32;
        let mut code = source.to_owned();
        code.replace_range(marker_start..marker_start + marker_len, "");

        (code, Position { line, character })
    }

    fn completion_labels_at_marker(source: &str) -> Vec<String> {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let file_path = root.join("main.scad");
        let (code, position) = source_and_position(source);
        fs::write(&file_path, &code).unwrap();

        let (mut server, client_conn) = make_server(root);
        let uri = Url::from_file_path(&file_path).unwrap();
        server.handle_completion(
            RequestId::from(1),
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            },
        );

        let message = client_conn.receiver.recv().expect("completion response");
        let Message::Response(response) = message else {
            panic!("expected response");
        };
        let result = response.result.expect("completion result");
        let response: CompletionResponse = serde_json::from_value(result).unwrap();
        let items = match response {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(list) => list.items,
        };

        items.into_iter().map(|item| item.label).collect()
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
    fn completion_in_empty_call_returns_callable_parameters() {
        let labels = completion_labels_at_marker(
            "module assembly(show_pill=true, show_barrel=true, my_param=4) {}\nassembly($);\n",
        );

        assert_eq!(labels, vec!["show_pill", "show_barrel", "my_param"]);
    }

    #[test]
    fn completion_in_call_argument_name_filters_by_prefix() {
        let labels = completion_labels_at_marker(
            "module assembly(show_pill=true, show_barrel=true, my_param=4) {}\nassembly(s$);\n",
        );

        assert_eq!(labels, vec!["show_pill", "show_barrel"]);
    }

    #[test]
    fn completion_in_argument_value_uses_lexical_items() {
        let labels = completion_labels_at_marker(
            "global_value = 1;\nmodule assembly(show_pill=true, show_barrel=true, my_param=4) {}\nassembly(show_barrel=$);\n",
        );

        assert!(labels.contains(&"global_value".to_owned()));
        assert!(labels.contains(&"assembly".to_owned()));
        assert!(!labels.contains(&"show_pill".to_owned()));
        assert!(!labels.contains(&"show_barrel".to_owned()));
    }

    #[test]
    fn completion_in_builtin_call_skips_positional_parameters() {
        let labels = completion_labels_at_marker("cube([10, 20, 5], $);\n");

        assert_eq!(labels, vec!["center"]);
    }

    #[test]
    fn completion_in_parameterless_builtin_call_is_empty() {
        let labels = completion_labels_at_marker("union($0) {}\n");

        assert!(labels.is_empty());
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
