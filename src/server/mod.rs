#[macro_use]
pub(crate) mod utils;
pub(crate) mod code_helper;
pub(crate) mod handler;
pub(crate) mod parse_code;
pub(crate) mod response_item;
pub(crate) mod workspace_index;

use directories::UserDirs;
use std::collections::HashSet;
use std::error::Error;
use std::fs::read_to_string;
use std::{cell::RefCell, env, path::PathBuf, rc::Rc};

use linked_hash_map::LinkedHashMap;
use lsp_server::{Connection, Message, Request, RequestId, Response};
use lsp_types::{
    HoverProviderCapability, InitializeParams, OneOf, RenameOptions, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, Url, WorkDoneProgressOptions,
};
use serde::Serialize;

use self::workspace_index::WorkspaceIndex;
use crate::Cli;
use crate::parse_code::ParsedCode;

const BUILTINS_SCAD: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/builtins"));
const BUILTIN_PATH: &str = "/builtin";

pub(crate) struct Server {
    pub library_locations: Rc<RefCell<Vec<Url>>>,

    pub connection: Connection,
    pub codes: LinkedHashMap<Url, Rc<RefCell<ParsedCode>>>,
    pub args: Cli,

    builtin_url: Url,
    fmt_query: Option<String>,
    pub(crate) workspace_roots: Vec<PathBuf>,
    pub(crate) open_documents: HashSet<Url>,
    pub(crate) workspace_index: WorkspaceIndex,
    did_change_watched_files_dynamic_registration: bool,
    did_change_watched_files_relative_pattern_support: bool,
    watched_files_registered: bool,
    pending_watched_files_registration: Option<RequestId>,
    next_request_id: i32,
}

pub(crate) enum LoopAction {
    Exit,
    Continue,
}

// Miscellaneous high-level logic.
impl Server {
    pub(crate) fn get_fmt_query(query_file: Option<PathBuf>) -> Option<String> {
        let query_file = query_file?;
        if query_file.as_os_str().is_empty() {
            return None;
        }
        match read_to_string(query_file) {
            Err(err) => {
                err_to_console!("failed to read file {:?}.", err);
                None
            }
            Ok(query) => Some(query),
        }
    }
    pub(crate) fn new(connection: Connection, args: Cli) -> Self {
        let builtin_path = PathBuf::from(&args.builtin);

        let fmt_query = Self::get_fmt_query(args.query_file.clone());
        let mut args = args;

        let mut code = BUILTINS_SCAD.to_owned();

        let mut external = false;
        match read_to_string(builtin_path) {
            Err(err) => {
                err_to_console!(
                    "failed to read external file of builtin-function, {:?}. will use the content included in binary.",
                    err
                );
                args.builtin = BUILTIN_PATH.to_owned();
            }
            Ok(builtin_str) => {
                code = builtin_str;
                external = true;
            }
        }

        let url = Url::parse(&format!("file://{}", &args.builtin)).unwrap();

        let mut instance = Self {
            library_locations: Rc::new(RefCell::new(vec![])),
            connection,
            codes: Default::default(),
            args,
            builtin_url: url.to_owned(),
            fmt_query,
            workspace_roots: Vec::new(),
            open_documents: HashSet::new(),
            workspace_index: WorkspaceIndex::default(),
            did_change_watched_files_dynamic_registration: false,
            did_change_watched_files_relative_pattern_support: false,
            watched_files_registered: false,
            pending_watched_files_registration: None,
            next_request_id: 1,
        };
        let rc = instance.insert_code(url, code);

        rc.borrow_mut().is_builtin = true;
        rc.borrow_mut().external_builtin = external;

        instance.make_library_locations();

        instance
    }

    pub(crate) fn user_defined_library_locations() -> Vec<String> {
        match env::var("OPENSCADPATH") {
            Ok(path) => env::split_paths(&path)
                .filter_map(|buf| buf.into_os_string().into_string().ok())
                .collect(),
            Err(_) => vec![],
        }
    }

    pub(crate) fn built_in_library_location() -> Option<String> {
        if let Some(userdir) = UserDirs::new() {
            let lib_path = if cfg!(target_os = "windows") {
                userdir
                    .document_dir()?
                    .join("OpenSCAD\\libraries\\")
                    .into_os_string()
                    .into_string()
            } else if cfg!(target_os = "macos") {
                userdir
                    .document_dir()?
                    .join("OpenSCAD/libraries/")
                    .into_os_string()
                    .into_string()
            } else {
                userdir
                    .home_dir()
                    .join(".local/share/OpenSCAD/libraries/")
                    .into_os_string()
                    .into_string()
            };

            return lib_path.ok();
        }

        None
    }

    pub(crate) fn installation_library_location() -> Option<String> {
        // TODO: Figure out the other cases.
        if cfg!(target_os = "windows") {
            Some("C:\\Program Files\\OpenSCAD\\libraries\\".into())
        } else if cfg!(target_os = "macos") {
            Some("/Applications/OpenSCAD.app/Contents/Resources/libraries/".into())
        } else {
            Some("/usr/share/openscad/libraries/".into())
        }
    }

    pub(crate) fn make_library_locations(&mut self) {
        let mut ret = Self::user_defined_library_locations();
        ret.extend(Self::built_in_library_location());
        ret.extend(Self::installation_library_location());

        self.extend_libs(ret);
    }

    pub(crate) fn extend_libs(&mut self, userlibs: Vec<String>) {
        let ret: Vec<Url> = userlibs
            .into_iter()
            .map(|lib| shellexpand::tilde(&lib).to_string())
            .filter_map(|p| {
                if p.is_empty() {
                    return None;
                }

                let mut path = format!("file://{p}");
                if !path.ends_with('/') {
                    path.push('/');
                }

                if let Ok(uri) = Url::parse(&path) {
                    if let Ok(path) = uri.to_file_path() {
                        if path.exists() {
                            return Some(uri);
                        }
                    }
                };

                None
            })
            .collect();

        if !ret.is_empty() {
            eprintln!();
            log_to_console!("search paths:");

            for lib in ret {
                log_to_console!("{}", &lib);
                if !self.library_locations.borrow().contains(&lib) {
                    self.library_locations.borrow_mut().push(lib);
                }
            }

            eprintln!();
        }
    }

    pub(crate) fn main_loop(&mut self) -> Result<(), Box<dyn Error + Sync + Send>> {
        let caps = serde_json::to_value(ServerCapabilities {
            text_document_sync: Some(TextDocumentSyncCapability::Kind(
                TextDocumentSyncKind::INCREMENTAL,
            )),
            completion_provider: Some(Default::default()),
            definition_provider: Some(OneOf::Left(true)),
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            document_symbol_provider: Some(OneOf::Left(true)),
            document_formatting_provider: Some(OneOf::Left(true)),
            rename_provider: Some(OneOf::Right(RenameOptions {
                prepare_provider: Some(true),
                work_done_progress_options: WorkDoneProgressOptions::default(),
            })),
            ..Default::default()
        })?;
        let init_params = self.connection.initialize(caps)?;
        let init: InitializeParams = serde_json::from_value(init_params)?;
        self.workspace_roots = Self::extract_workspace_roots(&init);
        self.configure_client_capabilities(&init);
        self.ensure_workspace_index();
        while let Ok(msg) = self.connection.receiver.recv() {
            match self.handle_message(msg)? {
                LoopAction::Continue => {}
                LoopAction::Exit => break,
            }
        }
        Ok(())
    }

    fn extract_workspace_roots(params: &InitializeParams) -> Vec<PathBuf> {
        let mut roots = Vec::new();

        #[allow(deprecated)]
        if let Some(root_uri) = params.root_uri.as_ref() {
            if let Ok(path) = root_uri.to_file_path() {
                if !roots.contains(&path) {
                    roots.push(path);
                }
            }
        } else if let Some(root_path) = {
            #[allow(deprecated)]
            {
                params.root_path.as_ref()
            }
        } {
            let path = PathBuf::from(root_path);
            if !roots.contains(&path) {
                roots.push(path);
            }
        }

        if let Some(folders) = &params.workspace_folders {
            for folder in folders {
                if let Ok(path) = folder.uri.to_file_path() {
                    if !roots.contains(&path) {
                        roots.push(path);
                    }
                }
            }
        }

        roots
    }

    pub(crate) fn configure_client_capabilities(&mut self, params: &InitializeParams) {
        let watched_files = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.did_change_watched_files);

        self.did_change_watched_files_dynamic_registration = watched_files
            .and_then(|capabilities| capabilities.dynamic_registration)
            .unwrap_or(false);
        self.did_change_watched_files_relative_pattern_support = watched_files
            .and_then(|capabilities| capabilities.relative_pattern_support)
            .unwrap_or(false);
    }

    pub(crate) fn supports_dynamic_watched_files_registration(&self) -> bool {
        self.did_change_watched_files_dynamic_registration
    }

    pub(crate) fn supports_relative_watch_patterns(&self) -> bool {
        self.did_change_watched_files_relative_pattern_support
    }

    pub(crate) fn watched_files_registered(&self) -> bool {
        self.watched_files_registered
    }

    pub(crate) fn watched_files_registration_pending(&self) -> bool {
        self.pending_watched_files_registration.is_some()
    }

    pub(crate) fn mark_watched_files_registration_pending(&mut self, request_id: RequestId) {
        self.pending_watched_files_registration = Some(request_id);
    }

    pub(crate) fn send_request<R>(&mut self, params: R::Params) -> RequestId
    where
        R: lsp_types::request::Request,
        R::Params: Serialize,
    {
        let request_id = RequestId::from(self.next_request_id);
        self.next_request_id += 1;

        self.connection
            .sender
            .send(Message::Request(Request::new(
                request_id.clone(),
                R::METHOD.to_owned(),
                params,
            )))
            .unwrap();

        request_id
    }

    pub(crate) fn handle_client_response(&mut self, response: Response) {
        if self.pending_watched_files_registration.as_ref() == Some(&response.id) {
            self.pending_watched_files_registration = None;

            if let Some(error) = response.error {
                self.watched_files_registered = false;
                err_to_console!(
                    "failed to register workspace file watchers: {} ({})",
                    error.message,
                    error.code
                );
            } else {
                self.watched_files_registered = true;
            }
            return;
        }

        err_to_console!("got response: {:?}", response);
    }
}
