use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs::read_to_string,
    io,
    rc::Rc,
};

use lsp_types::Url;
use tree_sitter::Node;

use crate::{
    parse_code::ParsedCode,
    response_item::{Item, ItemKind},
    server::Server,
    utils::*,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DependencyKind {
    Include,
    Use,
}

impl DependencyKind {
    fn nested_visibility(self, nested: DependencyKind) -> Option<Self> {
        match (self, nested) {
            (Self::Include, Self::Include) => Some(Self::Include),
            (Self::Include, Self::Use) => Some(Self::Use),
            (Self::Use, Self::Include) => Some(Self::Use),
            (Self::Use, Self::Use) => None,
        }
    }

    fn exports_only_callables(self) -> bool {
        matches!(self, Self::Use)
    }
}

type CacheKey = (Url, DependencyKind);
pub(crate) type IdentityCache = RefCell<HashMap<CacheKey, Vec<Rc<RefCell<Item>>>>>;

#[derive(Clone)]
struct DependencyEdge {
    url: Url,
    visibility: DependencyKind,
}

// Code-related helpers.
impl Server {
    pub(crate) fn get_code(&mut self, uri: &Url) -> Option<Rc<RefCell<ParsedCode>>> {
        match self.codes.get(uri) {
            Some(x) => Some(Rc::clone(x)),
            None => self.read_and_cache(uri.clone()).ok(),
        }
    }

    pub(crate) fn insert_code(&mut self, url: Url, code: String) -> Rc<RefCell<ParsedCode>> {
        while self.codes.len() > 1000 {
            self.codes.pop_front();
        }

        let rc = Rc::new(RefCell::new(ParsedCode::new(
            code,
            url.clone(),
            self.library_locations.clone(),
        )));
        self.codes.insert(url, rc.clone());
        rc
    }

    pub(crate) fn find_identities(
        &mut self,
        code: &ParsedCode,
        comparator: &dyn Fn(&str) -> bool,
        start_node: &Node,
        findall: bool,
    ) -> Vec<Rc<RefCell<Item>>> {
        let mut visited = HashSet::<CacheKey>::new();
        let depth_limit = if self.args.depth == 0 {
            None
        } else {
            Some(self.args.depth)
        };
        self.find_identities_inner(
            code,
            comparator,
            start_node,
            findall,
            &mut visited,
            true,
            depth_limit,
            DependencyKind::Include,
            None,
        )
    }

    pub(crate) fn find_identities_with_cache(
        &mut self,
        code: &ParsedCode,
        comparator: &dyn Fn(&str) -> bool,
        start_node: &Node,
        findall: bool,
        cache: &IdentityCache,
    ) -> Vec<Rc<RefCell<Item>>> {
        let mut visited = HashSet::<CacheKey>::new();
        let depth_limit = if self.args.depth == 0 {
            None
        } else {
            Some(self.args.depth)
        };
        self.find_identities_inner(
            code,
            comparator,
            start_node,
            findall,
            &mut visited,
            true,
            depth_limit,
            DependencyKind::Include,
            Some(cache),
        )
    }

    fn parameter_default_context(
        code: &ParsedCode,
        start_node: &Node,
    ) -> Option<(String, std::ops::Range<usize>)> {
        let mut node = *start_node;

        while let Some(parent) = node.parent() {
            if parent.kind() == "assignment" {
                let in_value = parent.child_by_field_name("value").is_some_and(|value| {
                    let value_range = value.byte_range();
                    let node_range = node.byte_range();
                    value_range.start <= node_range.start && node_range.end <= value_range.end
                });

                if in_value {
                    let Some(parameter) = parent.parent() else {
                        return None;
                    };
                    if parameter.kind() != "parameter" {
                        return None;
                    }

                    let Some(parameters) = parameter.parent() else {
                        return None;
                    };
                    if parameters.kind() != "parameters" {
                        return None;
                    }

                    let Some(owner) = parameters.parent() else {
                        return None;
                    };
                    if owner.kind() != "module_item" && owner.kind() != "function_item" {
                        return None;
                    }

                    let name_node = parent.child_by_field_name("name")?;
                    if name_node.kind() != "identifier" {
                        return None;
                    }

                    return Some((
                        node_text(&code.code, &name_node).to_owned(),
                        owner.byte_range(),
                    ));
                }
            }

            node = parent;
        }

        None
    }

    fn named_argument_parameter_context<'a>(
        code: &ParsedCode,
        start_node: &Node<'a>,
    ) -> Option<(Node<'a>, String)> {
        if start_node.kind() != "identifier" {
            return None;
        }

        let assignment = start_node.parent()?;
        if assignment.kind() != "assignment"
            || !assignment
                .child_by_field_name("name")
                .is_some_and(|name_node| name_node == *start_node)
        {
            return None;
        }

        let arguments = assignment.parent()?;
        if arguments.kind() != "arguments" {
            return None;
        }

        let call = arguments.parent()?;
        if call.kind() != "module_call" && call.kind() != "function_call" {
            return None;
        }

        Some((call, node_text(&code.code, start_node).to_owned()))
    }

    fn named_argument_parameter_identities(
        &mut self,
        code: &ParsedCode,
        comparator: &dyn Fn(&str) -> bool,
        start_node: &Node,
    ) -> Vec<Rc<RefCell<Item>>> {
        let Some((call, arg_name)) = Self::named_argument_parameter_context(code, start_node)
        else {
            return vec![];
        };

        if !comparator(&arg_name) {
            return vec![];
        }

        let Some(call_name_node) = call.child_by_field_name("name") else {
            return vec![];
        };
        let call_name = node_text(&code.code, &call_name_node).to_owned();

        let callable_items = self.find_identities(
            code,
            &|item_name| item_name == call_name.as_str(),
            &call,
            true,
        );

        for item in callable_items {
            let (params, url, is_builtin) = {
                let item_ref = item.borrow();
                let params = match &item_ref.kind {
                    ItemKind::Module { params } | ItemKind::Function { params } => params.clone(),
                    _ => continue,
                };
                (params, item_ref.url.clone(), item_ref.is_builtin)
            };

            if let Some(param) = params.into_iter().find(|param| param.name == arg_name) {
                return vec![Rc::new(RefCell::new(Item {
                    name: param.name,
                    kind: ItemKind::Variable,
                    range: param.range,
                    url,
                    is_builtin,
                    ..Default::default()
                }))];
            }
        }

        vec![]
    }

    fn find_identities_inner(
        &mut self,
        code: &ParsedCode,
        comparator: &dyn Fn(&str) -> bool,
        start_node: &Node,
        findall: bool,
        visited: &mut HashSet<CacheKey>,
        include_builtin: bool,
        remaining_depth: Option<usize>,
        visibility: DependencyKind,
        cache: Option<&IdentityCache>,
    ) -> Vec<Rc<RefCell<Item>>> {
        let mut result: Vec<Rc<RefCell<Item>>> = vec![];
        let code_key = (code.url.clone(), visibility);
        if !visited.insert(code_key) {
            return result;
        }

        let parameter_default_context = if findall {
            None
        } else {
            Self::parameter_default_context(code, start_node)
        };

        let named_argument_items =
            self.named_argument_parameter_identities(code, comparator, start_node);
        if !named_argument_items.is_empty() {
            if !findall {
                return named_argument_items;
            }
            result.extend(named_argument_items);
        }

        let mut dependency_vec = vec![];
        if include_builtin
            && !visited.contains(&(self.builtin_url.clone(), DependencyKind::Include))
        {
            dependency_vec.push(DependencyEdge {
                url: self.builtin_url.clone(),
                visibility: DependencyKind::Include,
            });
        }
        if let Some(incs) = &code.includes {
            dependency_vec.extend(
                incs.iter()
                    .filter_map(|inc| {
                        visibility
                            .nested_visibility(DependencyKind::Include)
                            .filter(|nested_visibility| {
                                !visited.contains(&(inc.clone(), *nested_visibility))
                            })
                            .map(|nested_visibility| DependencyEdge {
                                url: inc.clone(),
                                visibility: nested_visibility,
                            })
                    })
                    .collect::<Vec<_>>(),
            );
        }
        if let Some(uses) = &code.uses {
            dependency_vec.extend(
                uses.iter()
                    .filter_map(|url| {
                        visibility
                            .nested_visibility(DependencyKind::Use)
                            .filter(|nested_visibility| {
                                !visited.contains(&(url.clone(), *nested_visibility))
                            })
                            .map(|nested_visibility| DependencyEdge {
                                url: url.clone(),
                                visibility: nested_visibility,
                            })
                    })
                    .collect::<Vec<_>>(),
            );
        }

        let mut node = *start_node;
        let mut parent = start_node.parent();

        'outer: while parent.is_some() {
            let is_top_level_node = parent.unwrap().parent().is_none();

            loop {
                if node.kind().is_dependency_statement() {
                    let nested_kind = if node.kind().is_include_statement() {
                        DependencyKind::Include
                    } else {
                        DependencyKind::Use
                    };
                    code.get_include_url(&node).map(|url| {
                        visibility
                            .nested_visibility(nested_kind)
                            .map(|nested_visibility| {
                                dependency_vec.push(DependencyEdge {
                                    url,
                                    visibility: nested_visibility,
                                });
                            });
                    });
                }

                if let Some(mut item) = Item::parse(&code.code, &node) {
                    match &item.kind {
                        ItemKind::Module { params } => {
                            let skip_param_name = parameter_default_context.as_ref().and_then(
                                |(name, owner_range)| {
                                    (node.byte_range() == *owner_range).then_some(name.as_str())
                                },
                            );
                            for p in params {
                                if skip_param_name.is_some_and(|name| p.name == name) {
                                    continue;
                                }
                                if comparator(&p.name) {
                                    result.push(Rc::new(RefCell::new(Item {
                                        name: p.name.clone(),
                                        kind: ItemKind::Variable,
                                        range: p.range,
                                        url: Some(code.url.clone()),
                                        ..Default::default()
                                    })));
                                    if !findall {
                                        return result;
                                    }
                                }
                            }
                        }
                        ItemKind::Function { params } => {
                            let skip_param_name = parameter_default_context.as_ref().and_then(
                                |(name, owner_range)| {
                                    (node.byte_range() == *owner_range).then_some(name.as_str())
                                },
                            );
                            for p in params {
                                if skip_param_name.is_some_and(|name| p.name == name) {
                                    continue;
                                }
                                if comparator(&p.name) {
                                    result.push(Rc::new(RefCell::new(Item {
                                        name: p.name.clone(),
                                        kind: ItemKind::Variable,
                                        range: p.range,
                                        url: Some(code.url.clone()),
                                        ..Default::default()
                                    })));
                                    if !findall {
                                        return result;
                                    }
                                }
                            }
                        }
                        _ => {}
                    };

                    if visibility == DependencyKind::Include
                        && !is_top_level_node
                        && comparator(&item.name)
                    {
                        item.url = Some(code.url.clone());
                        result.push(Rc::new(RefCell::new(item)));
                        if !findall {
                            return result;
                        }
                    }
                }

                if is_top_level_node {
                    break 'outer;
                } else if node.prev_sibling().is_none() {
                    node = parent.unwrap();
                    parent = node.parent();
                    break;
                } else {
                    node = node.prev_sibling().unwrap();
                }
            }
        }

        if let Some(items) = &code.root_items {
            for item in items {
                let item_ref = item.borrow();
                let is_callable = matches!(
                    item_ref.kind,
                    ItemKind::Function { .. } | ItemKind::Module { .. }
                );
                if visibility.exports_only_callables() && !is_callable {
                    continue;
                }
                if comparator(&item_ref.name) {
                    result.push(item.clone());
                    if !findall {
                        return result;
                    }
                }
            }
        }

        for edge in dependency_vec {
            let cache_key = (edge.url.clone(), edge.visibility);
            if visited.contains(&cache_key) || matches!(remaining_depth, Some(0)) {
                continue;
            }

            if let Some(cache_cell) = cache {
                if !findall {
                    if let Some(cached) = cache_cell.borrow().get(&cache_key).cloned() {
                        visited.insert(cache_key.clone());
                        result.extend(cached);
                        if !result.is_empty() && !findall {
                            return result;
                        }
                        continue;
                    }
                }
            }

            let inccode = match self.get_code(&edge.url) {
                Some(code) => code,
                _ => return result,
            };

            if let Ok(mut inccode) = inccode.try_borrow_mut() {
                inccode.gen_top_level_items_if_needed();
                let next_depth = remaining_depth.map(|depth| depth - 1);
                let nested = self.find_identities_inner(
                    &inccode,
                    comparator,
                    &inccode.tree.root_node(),
                    findall,
                    visited,
                    false,
                    next_depth,
                    edge.visibility,
                    cache,
                );
                if let Some(cache_cell) = cache {
                    if !findall {
                        cache_cell.borrow_mut().insert(cache_key, nested.clone());
                    }
                }
                result.extend(nested);
            }

            if !result.is_empty() && !findall {
                return result;
            }
        }

        result
    }

    pub(crate) fn read_and_cache(&mut self, url: Url) -> io::Result<Rc<RefCell<ParsedCode>>> {
        let text = read_to_string(url.to_file_path().unwrap())?;

        match self.codes.entry(url.clone()) {
            linked_hash_map::Entry::Occupied(o) => {
                if o.get().borrow().code != text {
                    Ok(self.insert_code(url, text))
                } else {
                    Ok(Rc::clone(o.get()))
                }
            }
            linked_hash_map::Entry::Vacant(_) => Ok(self.insert_code(url, text)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cli;
    use clap::Parser;
    use lsp_server::Connection;
    use lsp_types::Url;
    use std::fs;
    use tempfile::tempdir;
    use tree_sitter_traversal2::{Order, traverse};

    #[test]
    fn find_identities_traverses_deep_include_graph() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        let files = [
            "top.scad",
            "lvl1.scad",
            "lvl2.scad",
            "lvl3.scad",
            "lvl4.scad",
        ];

        for window in files.windows(2) {
            let current = root.join(window[0]);
            let next = window[1];
            let content = format!("include <{next}>;\n");
            fs::write(current, content).unwrap();
        }

        let leaf_path = root.join("lvl4.scad");
        fs::write(&leaf_path, "module nested() {}\n").unwrap();

        let top_path = root.join("top.scad");
        let top_content = "include <lvl1.scad>;\nmodule top_use() { nested(); }\n";
        fs::write(&top_path, top_content).unwrap();

        let (server_conn, _client_conn) = Connection::memory();
        let args = Cli::parse_from(["openscad-lsp"]);
        let mut server = Server::new(server_conn, args);

        let top_url = Url::from_file_path(&top_path).unwrap();
        let top_code = server.get_code(&top_url).expect("load top.scad");
        if let Ok(mut code_mut) = top_code.try_borrow_mut() {
            code_mut.gen_top_level_items_if_needed();
        }
        let top_ref = top_code.borrow();

        let cursor = top_ref.tree.walk();
        let mut iter = traverse(cursor, Order::Pre);
        let mut call_node = None;
        while let Some(node) = iter.next() {
            if node.kind() == "identifier" && super::node_text(&top_ref.code, &node) == "nested" {
                call_node = Some(node);
                break;
            }
        }
        let call_node = call_node.expect("call site");

        let results = server.find_identities(&top_ref, &|name| name == "nested", &call_node, false);

        let leaf_url = Url::from_file_path(&leaf_path).unwrap();
        assert!(
            results
                .iter()
                .any(|item| item.borrow().url.as_ref() == Some(&leaf_url)),
            "expected nested definition in deepest include"
        );
    }

    #[test]
    fn parameter_default_self_reference_resolves_to_outer_variable() {
        let tmp = tempdir().unwrap();
        let file_path = tmp.path().join("main.scad");
        let source = "show_ackermann_plate = false;\nmodule upper_chassis(show_ackermann_plate=show_ackermann_plate) {}\n";
        fs::write(&file_path, source).unwrap();

        let (server_conn, _client_conn) = Connection::memory();
        let args = Cli::parse_from(["openscad-lsp"]);
        let mut server = Server::new(server_conn, args);

        let file_url = Url::from_file_path(&file_path).unwrap();
        let parsed = server.get_code(&file_url).expect("load test file");
        if let Ok(mut parsed_mut) = parsed.try_borrow_mut() {
            parsed_mut.gen_top_level_items_if_needed();
        }
        let parsed_ref = parsed.borrow();

        let mut rhs_node = None;
        let mut outer_var_range = None;

        let mut iter = traverse(parsed_ref.tree.walk(), Order::Pre);
        while let Some(node) = iter.next() {
            if node.kind() != "identifier"
                || super::node_text(&parsed_ref.code, &node) != "show_ackermann_plate"
            {
                continue;
            }

            if node.parent().is_some_and(|assignment| {
                assignment.kind() == "assignment"
                    && assignment
                        .child_by_field_name("value")
                        .is_some_and(|value| {
                            let value_range = value.byte_range();
                            let node_range = node.byte_range();
                            value_range.start <= node_range.start
                                && node_range.end <= value_range.end
                        })
                    && assignment
                        .parent()
                        .is_some_and(|parameter| parameter.kind() == "parameter")
            }) {
                rhs_node = Some(node);
            }

            if let Some(assignment) = node.parent() {
                if assignment.kind() == "assignment"
                    && assignment
                        .child_by_field_name("name")
                        .is_some_and(|name_node| name_node == node)
                {
                    if let Some(decl) = assignment.parent() {
                        if decl.kind() == "var_declaration" {
                            outer_var_range = Some(assignment.lsp_range());
                        }
                    }
                }
            }
        }

        let rhs_node = rhs_node.expect("rhs identifier in parameter default");
        let outer_var_range = outer_var_range.expect("outer variable declaration");
        let results = server.find_identities(
            &parsed_ref,
            &|name| name == "show_ackermann_plate",
            &rhs_node,
            false,
        );

        let first = results.first().expect("definition result");
        let first = first.borrow();
        assert_eq!(first.url.as_ref(), Some(&file_url));
        assert_eq!(first.range, outer_var_range);
    }

    #[test]
    fn later_parameter_default_can_resolve_to_earlier_parameter() {
        let tmp = tempdir().unwrap();
        let file_path = tmp.path().join("main.scad");
        let source = "module m(a=1, b=a) {}\n";
        fs::write(&file_path, source).unwrap();

        let (server_conn, _client_conn) = Connection::memory();
        let args = Cli::parse_from(["openscad-lsp"]);
        let mut server = Server::new(server_conn, args);

        let file_url = Url::from_file_path(&file_path).unwrap();
        let parsed = server.get_code(&file_url).expect("load test file");
        if let Ok(mut parsed_mut) = parsed.try_borrow_mut() {
            parsed_mut.gen_top_level_items_if_needed();
        }
        let parsed_ref = parsed.borrow();

        let mut rhs_a_node = None;
        let mut param_a_range = None;

        let mut iter = traverse(parsed_ref.tree.walk(), Order::Pre);
        while let Some(node) = iter.next() {
            if node.kind() != "identifier" || super::node_text(&parsed_ref.code, &node) != "a" {
                continue;
            }

            if node.parent().is_some_and(|assignment| {
                assignment.kind() == "assignment"
                    && assignment
                        .child_by_field_name("value")
                        .is_some_and(|value| {
                            let value_range = value.byte_range();
                            let node_range = node.byte_range();
                            value_range.start <= node_range.start
                                && node_range.end <= value_range.end
                        })
                    && assignment
                        .child_by_field_name("name")
                        .is_some_and(|name_node| {
                            super::node_text(&parsed_ref.code, &name_node) == "b"
                        })
            }) {
                rhs_a_node = Some(node);
            }

            if node.parent().is_some_and(|assignment| {
                assignment.kind() == "assignment"
                    && assignment
                        .child_by_field_name("name")
                        .is_some_and(|name_node| name_node == node)
                    && assignment
                        .parent()
                        .is_some_and(|parameter| parameter.kind() == "parameter")
            }) {
                param_a_range = Some(node.lsp_range());
            }
        }

        let rhs_a_node = rhs_a_node.expect("rhs a in second default parameter");
        let param_a_range = param_a_range.expect("first parameter declaration");
        let results = server.find_identities(&parsed_ref, &|name| name == "a", &rhs_a_node, false);

        let first = results.first().expect("definition result");
        let first = first.borrow();
        assert_eq!(first.url.as_ref(), Some(&file_url));
        assert_eq!(first.range, param_a_range);
    }
}
