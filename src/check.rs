use std::{
    fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
};

use lsp_server::Connection;
use lsp_types::{DiagnosticSeverity, NumberOrString, Url};

use crate::{
    Cli,
    server::{Server, diagnostics::document_diagnostics},
};

pub(crate) fn run(args: Cli, path: &Path) -> ExitCode {
    let files = match collect_files(path) {
        Ok(files) => files,
        Err(error) => {
            eprintln!("{}: error: {error}", path.display());
            return ExitCode::from(2);
        }
    };
    let (connection, _client) = Connection::memory();
    let mut server = Server::new(connection, args);
    let mut checked = 0;
    let mut warnings = 0;
    let mut errors = 0;
    let mut read_errors = 0;

    for path in files {
        let url = match Url::from_file_path(&path) {
            Ok(url) => url,
            Err(()) => {
                eprintln!(
                    "{}: error: cannot represent path as a file URL",
                    path.display()
                );
                read_errors += 1;
                continue;
            }
        };
        let Some(code) = server.get_code(&url) else {
            eprintln!("{}: error: cannot read UTF-8 source file", path.display());
            read_errors += 1;
            continue;
        };
        let code = code.borrow();
        let mut diagnostics = document_diagnostics(&code);
        diagnostics.extend(server.unused_use_diagnostics(&code));
        diagnostics.sort_by_key(|diagnostic| {
            (
                diagnostic.range.start.line,
                diagnostic.range.start.character,
            )
        });
        for diagnostic in diagnostics {
            let severity = if diagnostic.severity == Some(DiagnosticSeverity::ERROR) {
                errors += 1;
                "error"
            } else {
                warnings += 1;
                "warning"
            };
            let diagnostic_code = match diagnostic.code {
                Some(NumberOrString::String(code)) => format!(" [{code}]"),
                Some(NumberOrString::Number(code)) => format!(" [{code}]"),
                None => String::new(),
            };
            println!(
                "{}:{}:{}: {severity}: {}{diagnostic_code}",
                path.display(),
                diagnostic.range.start.line + 1,
                diagnostic.range.start.character + 1,
                diagnostic.message.replace(['\n', '\r'], " "),
            );
        }
        checked += 1;
    }

    eprintln!(
        "Checked {checked} files: {errors} errors, {warnings} warnings, {read_errors} unreadable files."
    );
    if read_errors > 0 {
        ExitCode::from(2)
    } else if errors > 0 || warnings > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn collect_files(path: &Path) -> io::Result<Vec<PathBuf>> {
    let path = fs::canonicalize(path)?;
    if path.is_file() {
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("scad"))
        {
            return Ok(vec![path]);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a .scad file or directory",
        ));
    }
    if !path.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a .scad file or directory",
        ));
    }

    let mut files = Vec::new();
    for entry in Server::walk_scad_files(&path) {
        let entry = entry.map_err(io::Error::other)?;
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            files.push(entry.into_path());
        }
    }
    files.sort();
    Ok(files)
}
