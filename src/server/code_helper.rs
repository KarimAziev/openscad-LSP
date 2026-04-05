use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
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
        let mut visited = HashSet::new();
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
            None,
        )
    }

    pub(crate) fn find_identities_with_cache(
        &mut self,
        code: &ParsedCode,
        comparator: &dyn Fn(&str) -> bool,
        start_node: &Node,
        findall: bool,
        cache: &RefCell<HashMap<Url, Vec<Rc<RefCell<Item>>>>>,
    ) -> Vec<Rc<RefCell<Item>>> {
        let mut visited = HashSet::new();
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

    fn find_identities_inner(
        &mut self,
        code: &ParsedCode,
        comparator: &dyn Fn(&str) -> bool,
        start_node: &Node,
        findall: bool,
        visited: &mut HashSet<Url>,
        include_builtin: bool,
        remaining_depth: Option<usize>,
        cache: Option<&RefCell<HashMap<Url, Vec<Rc<RefCell<Item>>>>>>,
    ) -> Vec<Rc<RefCell<Item>>> {
        let mut result: Vec<Rc<RefCell<Item>>> = vec![];
        if !visited.insert(code.url.clone()) {
            return result;
        }

        let parameter_default_context = if findall {
            None
        } else {
            Self::parameter_default_context(code, start_node)
        };

        let mut include_vec = vec![];
        if include_builtin && !visited.contains(&self.builtin_url) {
            include_vec.push(self.builtin_url.clone());
        }
        if let Some(incs) = &code.includes {
            include_vec.extend(incs.iter().filter(|inc| !visited.contains(*inc)).cloned());
        }

        let mut node = *start_node;
        let mut parent = start_node.parent();

        'outer: while parent.is_some() {
            let is_top_level_node = parent.unwrap().parent().is_none();

            loop {
                if node.kind().is_include_statement() {
                    code.get_include_url(&node).map(|inc| {
                        include_vec.push(inc);
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

                    if !is_top_level_node && comparator(&item.name) {
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
                if comparator(&item.borrow().name) {
                    result.push(item.clone());
                    if !findall {
                        return result;
                    }
                }
            }
        }

        for inc in include_vec {
            if visited.contains(&inc) {
                continue;
            }
            if let Some(0) = remaining_depth {
                continue;
            }

            if let Some(cache_cell) = cache {
                if !findall {
                    if let Some(cached) = cache_cell.borrow().get(&inc).cloned() {
                        visited.insert(inc.clone());
                        result.extend(cached);
                        if !result.is_empty() && !findall {
                            return result;
                        }
                        continue;
                    }
                }
            }

            let inccode = match self.get_code(&inc) {
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
                    cache,
                );
                if let Some(cache_cell) = cache {
                    if !findall {
                        cache_cell.borrow_mut().insert(inc.clone(), nested.clone());
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

    pub(crate) fn collect_related_urls(&mut self, seeds: &[Url]) -> HashSet<Url> {
        let mut visited = HashSet::new();
        let mut queue: VecDeque<Url> = VecDeque::new();

        for seed in seeds {
            queue.push_back(seed.clone());
        }

        while let Some(url) = queue.pop_front() {
            if !visited.insert(url.clone()) {
                continue;
            }

            if let Some(code_rc) = self.get_code(&url) {
                if let Ok(mut code_mut) = code_rc.try_borrow_mut() {
                    code_mut.gen_top_level_items_if_needed();
                    if let Some(includes) = &code_mut.includes {
                        for inc in includes {
                            if !visited.contains(inc) {
                                queue.push_back(inc.clone());
                            }
                        }
                    }
                }
            }
        }

        visited
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
