// See src/lib.rs: panicking accessors are banned in non-test code.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
    )
)]
#![deny(unsafe_code)]

use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process;

use clap::{Args, CommandFactory, Parser, Subcommand};

use al::cli::{help, man};
use al::stdlib;
use al::{ast, bytecode, diagnostic, formatter, lsp, parser, repl, scanner, vm};

const VERSION: &str = include_str!("../../../VERSION");

#[derive(Parser)]
#[command(
    name = "al",
    version = VERSION.trim(),
    about = "A small, expressive programming language",
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start an interactive REPL session
    Repl,
    /// Start the Language Server Protocol server
    Lsp,
    /// Type check a program without running it
    Check { entrypoint: String },
    /// Parse and print the AST of a program
    Build { entrypoint: String },
    /// Format AL source files
    Fmt(FmtArgs),
    /// Upgrade to a specific version (default: canary)
    Upgrade { version: Option<String> },
    /// Run a program
    Run(RunArgs),
}

#[derive(Args)]
struct RunArgs {
    entrypoint: String,
    /// Print the parsed program before execution starts
    #[arg(long = "debug-printer")]
    debug_printer: bool,
}

#[derive(Args)]
struct FmtArgs {
    path: Option<String>,
    /// Print formatted output instead of writing to files
    #[arg(long)]
    stdout: bool,
    /// Read input from stdin instead of a file
    #[arg(long)]
    stdin: bool,
    /// Check if files are formatted (exit 1 if not)
    #[arg(long)]
    check: bool,
    /// Print debug information about tokens
    #[arg(long)]
    debug: bool,
}

/// Print diagnostics (if any) and exit when `fail` is set.
fn report(diagnostics: &[diagnostic::Diagnostic], fail: bool, file: &str, entrypoint: &str) {
    if !diagnostics.is_empty() {
        diagnostic::print_diagnostics(diagnostics, file, entrypoint);
        if fail {
            process::exit(1);
        }
    }
}

fn parse_source(file: &str, entrypoint: &str) -> ast::Expression {
    let mut s = scanner::new_scanner(file.to_string());
    let mut p = parser::new_parser(&mut s);
    let result = p.parse_program();

    let fail = diagnostic::has_errors(&result.diagnostics);
    report(&result.diagnostics, fail, file, entrypoint);

    ast::Expression::BlockExpression(result.ast)
}

type CompileFn = fn(
    &ast::Expression,
    Option<&Path>,
    Option<&'static al::StaticStdlib>,
) -> bytecode::CompileResult;

fn compile_source(
    expr: &ast::Expression,
    file: &str,
    entrypoint: &str,
    f: CompileFn,
) -> bytecode::CompileResult {
    let path = Path::new(entrypoint);
    let base_dir = path.parent();
    // Editing the AL repo's own stdlib: analyse as that module so `@vm`/
    // external are permitted and prelude self-redefinition is suppressed.
    let result = match al::module::detect_stdlib_module(path) {
        Some(m) => bytecode::check_as_module(expr, base_dir, m),
        None => f(expr, base_dir, Some(stdlib())),
    };

    report(&result.diagnostics, !result.success, file, entrypoint);

    result
}

fn find_al_files(path: &str) -> io::Result<Vec<PathBuf>> {
    let p = Path::new(path);
    if p.is_file() {
        if path.ends_with(".al") {
            return Ok(vec![p.to_path_buf()]);
        }
        return Ok(vec![]);
    }

    if !p.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Path does not exist: {path}"),
        ));
    }

    let mut files = Vec::new();
    al::module::collect_al_files(p, &mut files);
    Ok(files)
}

struct FormatFileResult {
    changed: bool,
    output: String,
    has_errors: bool,
    errors: Vec<String>,
}

// BUGFIX: V's main.v uses 0-indexed line/col for files but +1 for stdin; the
// rust port emits 1-indexed consistently.
fn render_fmt_diagnostic(path: impl std::fmt::Display, d: &diagnostic::Diagnostic) -> String {
    let line = d.span.start_line + 1;
    let col = d.span.start_column + 1;
    format!("{path}:{line}:{col}: {}", d.message)
}

fn format_file(path: &Path, debug: bool) -> io::Result<FormatFileResult> {
    let content = fs::read_to_string(path)?;
    let result = formatter::format_with_debug(&content, debug);

    if result.has_errors {
        return Ok(FormatFileResult {
            changed: false,
            output: content,
            has_errors: true,
            errors: result
                .diagnostics
                .iter()
                .map(|d| render_fmt_diagnostic(path.display(), d))
                .collect(),
        });
    }

    Ok(FormatFileResult {
        changed: result.output != content,
        output: result.output,
        has_errors: false,
        errors: vec![],
    })
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("{msg}");
    process::exit(1);
}

fn read_file_or_die(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| die(e))
}

fn main() {
    // clap is the parser and command model only — all help/version/error/man
    // output is rendered by `al::cli`. Intercept the meta flags before clap so
    // they work without satisfying required args (`al run --help`).
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let cmd = Cli::command();

    if raw.iter().any(|a| a == "-V" || a == "--version") {
        help::version(&cmd);
        return;
    }

    let wants_help = raw.iter().any(|a| a == "-h" || a == "--help");
    let help_word = raw.first().map(String::as_str) == Some("help");
    if wants_help || help_word {
        let target = raw
            .iter()
            .find(|a| !a.starts_with('-') && a.as_str() != "help")
            .map(String::as_str);
        help::help(&cmd, target);
        return;
    }

    if raw.is_empty() {
        help::home(&cmd);
        return;
    }

    if raw.first().map(String::as_str) == Some("man") {
        if let Err(e) = man::render(&cmd) {
            die(format!("could not render man page: {e}"));
        }
        return;
    }

    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            help::error(&e);
            process::exit(2);
        }
    };

    match cli.command {
        None => {
            help::home(&cmd);
        }
        Some(Commands::Repl) => {
            repl::run(VERSION.trim());
        }
        Some(Commands::Lsp) => {
            let mut server = lsp::new_server();
            server.run();
        }
        Some(Commands::Check { entrypoint }) => {
            let file = read_file_or_die(&entrypoint);
            let expr = parse_source(&file, &entrypoint);
            compile_source(&expr, &file, &entrypoint, bytecode::check);
        }
        Some(Commands::Build { entrypoint }) => {
            let file = read_file_or_die(&entrypoint);
            let r = formatter::format_with_debug(&file, false);
            if r.has_errors {
                diagnostic::print_diagnostics(&r.diagnostics, &file, &entrypoint);
                process::exit(1);
            }
            println!("{}", r.output);
        }
        Some(Commands::Upgrade { version }) => {
            if let Err(e) = cmd_upgrade(version) {
                die(format!("upgrade failed: {e}"));
            }
        }
        Some(Commands::Run(args)) => {
            cmd_run(args);
        }
        Some(Commands::Fmt(args)) => {
            cmd_fmt(args);
        }
    }
}

fn cmd_run(args: RunArgs) {
    let file = read_file_or_die(&args.entrypoint);
    let expr = parse_source(&file, &args.entrypoint);

    if args.debug_printer {
        println!();
        println!("================DEBUG: Printed parsed source code================");
        println!("{}", formatter::format_with_debug(&file, false).output);
        println!("=================================================================");
        println!();
    }

    let result = compile_source(&expr, &file, &args.entrypoint, bytecode::compile);

    let mut v = vm::new_vm(result.program).unwrap_or_else(|e| die(e));
    let run_result = v.run().unwrap_or_else(|e| die(e));

    if !matches!(run_result.as_enum(), Some(e) if e.type_id() == al::stdlib::prelude::NIL.type_id) {
        println!("{}", vm::inspect(&run_result, v.program()));
    }
}

fn cmd_fmt(args: FmtArgs) {
    if args.stdin {
        let mut content = String::new();
        if let Err(e) = io::stdin().read_to_string(&mut content) {
            die(format!("Error reading stdin: {e}"));
        }
        let result = formatter::format_with_debug(&content, args.debug);
        if result.has_errors {
            for d in &result.diagnostics {
                eprintln!("{}", render_fmt_diagnostic("stdin", d));
            }
            process::exit(1);
        }
        print!("{}", result.output);
        let _ = io::stdout().flush();
        return;
    }

    let path = args.path.as_deref().unwrap_or(".");

    let files = find_al_files(path).unwrap_or_else(|e| die(e));

    if files.is_empty() {
        println!("No .al files found");
        return;
    }

    let mut needs_formatting = false;
    let mut has_errors = false;

    for file in &files {
        let result = match format_file(file, args.debug) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Error formatting {}: {e}", file.display());
                has_errors = true;
                continue;
            }
        };

        if result.has_errors {
            for err_msg in &result.errors {
                eprintln!("{err_msg}");
            }
            has_errors = true;
            continue;
        }

        if args.check {
            if result.changed {
                println!("{} needs formatting", file.display());
                needs_formatting = true;
            }
        } else if args.stdout {
            print!("{}", result.output);
        } else if result.changed {
            if let Err(e) = fs::write(file, &result.output) {
                eprintln!("Error writing {}: {e}", file.display());
                has_errors = true;
                continue;
            }
            println!("Formatted {}", file.display());
        }
    }

    if has_errors {
        process::exit(1);
    }

    if args.check && needs_formatting {
        process::exit(1);
    }
}

fn cmd_upgrade(version: Option<String>) -> Result<(), String> {
    let current_exe =
        std::env::current_exe().map_err(|e| format!("cannot locate current executable: {e}"))?;

    let tag = match version.as_deref() {
        None => "canary".to_string(),
        Some(v) if v == "canary" || v.contains("canary") => v.to_string(),
        Some(v) if v.chars().next().is_some_and(|c| c.is_ascii_digit()) => format!("v{v}"),
        Some(v) => v.to_string(),
    };

    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x86_64"
    };
    let os_name = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        return Err("unsupported OS".to_string());
    };

    let asset_name = format!("al-{os_name}-{arch}");
    let tmp_path = std::env::temp_dir().join(&asset_name);
    let download_url = format!("https://github.com/alii/al/releases/download/{tag}/{asset_name}");

    println!("Downloading {tag}...");

    let resp = ureq::get(&download_url)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let len: u64 = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut reader = resp.into_reader();
    let mut out = fs::File::create(&tmp_path)
        .map_err(|e| format!("cannot create {}: {e}", tmp_path.display()))?;
    let mut buf = [0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("read error: {e}"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])
            .map_err(|e| format!("write error: {e}"))?;
        written += n as u64;
        if len > 0 {
            eprint!("\r  {:>6.1} / {:.1} MB", mb(written), mb(len));
        } else {
            eprint!("\r  {:>6.1} MB", mb(written));
        }
    }
    eprintln!();
    drop(out);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod failed: {e}"))?;
    }

    fs::rename(&tmp_path, &current_exe)
        .map_err(|e| format!("cannot replace {}: {e}", current_exe.display()))?;

    match process::Command::new(&current_exe)
        .arg("--version")
        .output()
    {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout);
            let v = v.trim().strip_prefix("al ").unwrap_or(v.trim());
            println!("Upgraded to {v}");
        }
        _ => println!("Upgraded successfully!"),
    }
    Ok(())
}

#[inline]
fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}
