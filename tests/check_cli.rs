use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::tempdir;

fn check(path: &Path, options: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_openscad-lsp"))
        .arg("--check")
        .arg(path)
        .args(options)
        .output()
        .expect("run checker")
}

#[test]
fn directory_check_reports_diagnostics_and_respects_ignored_paths() {
    let directory = tempdir().unwrap();
    let root = directory.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("nested")).unwrap();
    fs::create_dir(root.join("target")).unwrap();
    fs::write(root.join(".gitignore"), "ignored.scad\n").unwrap();
    fs::write(root.join(".ignore"), "also_ignored.scad\n").unwrap();
    for name in [
        "ignored.scad",
        "also_ignored.scad",
        ".hidden.scad",
        "target/build.scad",
    ] {
        fs::write(root.join(name), "include <missing.scad>").unwrap();
    }
    fs::write(root.join("lib.scad"), "module shape() {}").unwrap();
    fs::write(root.join("main file.scad"), "use <lib.scad>\ncube(1);").unwrap();
    fs::write(
        root.join("nested/part.scad"),
        "module wrapper() { unused = 1; }",
    )
    .unwrap();

    let result = check(root, &[]);
    assert_eq!(result.status.code(), Some(1));
    let stdout = String::from_utf8(result.stdout).unwrap();
    let path = fs::canonicalize(root.join("main file.scad")).unwrap();
    assert!(stdout.contains(&format!(
        "{}:1:1: warning: unused use directive `<lib.scad>` [unused-use]",
        path.display()
    )));
    assert!(stdout.contains("unused local variable `unused`"));
    assert_eq!(stdout.lines().count(), 2, "{stdout}");
    assert_eq!(
        String::from_utf8(result.stderr).unwrap(),
        "Checked 3 files: 0 errors, 2 warnings, 0 unreadable files.\n"
    );
}

#[test]
fn single_file_check_succeeds_when_import_is_used() {
    let directory = tempdir().unwrap();
    fs::write(directory.path().join("lib.scad"), "module shape() {}").unwrap();
    let path = directory.path().join("main.scad");
    fs::write(&path, "use <lib.scad>\nshape();").unwrap();
    let result = check(&path, &[]);
    assert!(result.status.success());
    assert!(result.stdout.is_empty());
    assert_eq!(
        String::from_utf8(result.stderr).unwrap(),
        "Checked 1 files: 0 errors, 0 warnings, 0 unreadable files.\n"
    );
}

#[test]
fn check_reports_syntax_and_missing_dependency_errors() {
    let directory = tempdir().unwrap();
    fs::write(
        directory.path().join("missing.scad"),
        "include <absent.scad>",
    )
    .unwrap();
    fs::write(directory.path().join("syntax.scad"), "module broken( {").unwrap();
    let result = check(directory.path(), &[]);
    assert_eq!(result.status.code(), Some(1));
    let stdout = String::from_utf8(result.stdout).unwrap();
    assert!(
        stdout.contains("missing.scad:1:10: error: file not found!"),
        "{stdout}"
    );
    assert!(
        stdout.contains("syntax.scad:") && stdout.contains("syntax error"),
        "{stdout}"
    );
}

#[test]
fn check_exit_status_distinguishes_input_errors() {
    let directory = tempdir().unwrap();
    assert_eq!(
        check(&directory.path().join("missing"), &[]).status.code(),
        Some(2)
    );
    let path = directory.path().join("invalid.scad");
    fs::write(&path, [0xff]).unwrap();
    let result = check(&path, &[]);
    assert_eq!(result.status.code(), Some(2));
    assert!(
        String::from_utf8(result.stderr)
            .unwrap()
            .contains("1 unreadable files")
    );
    let path = directory.path().join("notes.txt");
    fs::write(&path, "notes").unwrap();
    assert_eq!(check(&path, &[]).status.code(), Some(2));
}

#[test]
fn check_supports_opt_in_include_analysis() {
    let directory = tempdir().unwrap();
    fs::write(directory.path().join("lib.scad"), "module shape() {}").unwrap();
    fs::write(directory.path().join("body.scad"), "shape();").unwrap();
    let path = directory.path().join("main.scad");
    fs::write(&path, "use <lib.scad>\ninclude <body.scad>").unwrap();
    assert_eq!(check(&path, &[]).status.code(), Some(1));
    assert!(check(&path, &["--unused-use-includes"]).status.success());
    assert_eq!(check(&path, &["--stdio"]).status.code(), Some(2));
}
