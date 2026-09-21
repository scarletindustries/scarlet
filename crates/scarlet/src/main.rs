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

use scarlet::cli::{help, man};
use scarlet::{ast, bytecode, diagnostic, dis, formatter, lint, lsp, parser, repl, scanner};

const VERSION: &str = env!("SCARLET_VERSION");

#[derive(Parser)]
#[command(
    name = "scarlet",
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
    /// Deprecated alias for `fmt --stdout`
    #[command(hide = true)]
    Build { entrypoint: String },
    /// Format Scarlet source files
    Fmt(FmtArgs),
    /// Survey source files for illegal-state shapes
    Lint(LintArgs),
    /// Upgrade to a specific version (default: canary)
    Upgrade { version: Option<String> },
    /// Print the compiled Core IR
    Dis(DisArgs),
    /// Run a program
    Run(RunArgs),
}

#[derive(Args)]
struct DisArgs {
    entrypoint: String,
    /// Every function, from any module, whose name contains this. Without it
    /// only this file's functions and toplevel are printed.
    #[arg(long = "fn", value_name = "NAME")]
    only: Option<String>,
}

#[derive(Args)]
struct RunArgs {
    entrypoint: String,
    /// Print the parsed program before execution starts
    #[arg(long = "debug-printer")]
    debug_printer: bool,
    /// Arguments passed through to the program, readable via `os.argv`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args)]
struct LintArgs {
    /// Paths to survey (default: the working directory). Taken as given, so
    /// this can be pointed at another `.scrl` corpus.
    paths: Vec<PathBuf>,
    /// Report constructors carrying at least this many `Bool` fields.
    #[arg(
        long = "min-bools",
        value_name = "N",
        default_value_t = lint::DEFAULT_MIN_BOOLS,
        value_parser = parse_min_bools,
    )]
    min_bools: usize,
}

/// Below the floor every constructor with a `Bool` qualifies, so the census
/// would report its own corpus back rather than a shape worth reading.
fn parse_min_bools(text: &str) -> Result<usize, String> {
    match text.parse::<usize>() {
        Ok(n) if n >= lint::DEFAULT_MIN_BOOLS => Ok(n),
        Ok(n) => Err(format!("{n} is below {}", lint::DEFAULT_MIN_BOOLS)),
        Err(e) => Err(e.to_string()),
    }
}

/// What `al fmt` does with each file it formatted.
#[derive(Clone, Copy)]
enum FileAction {
    /// Rewrite the file, when formatting changed it.
    WriteBack,
    /// `--stdout`: print the formatted text and leave the file alone.
    Print,
    /// `--check`: name the files that need formatting and exit 1 if any do.
    Check,
}

/// What `al fmt` was pointed at, and what to do with it.
///
/// The three flags that select these are mutually exclusive, so the parse
/// boundary is the last place a combination of them can be expressed:
/// [`FmtArgs::target`] is what `cmd_fmt` reads. `--stdin --stdout` used to
/// parse and then silently ignore `--stdout`, and `path` was only excluded
/// alongside `--stdin` by a clap attribute; neither has a spelling here.
enum FmtTarget {
    /// `--stdin`: format stdin and print the result. Takes no path.
    Stdin,
    /// Walk `path` (default `.`) for `.scrl` files and apply `action` to each.
    Files {
        path: Option<String>,
        action: FileAction,
    },
}

struct FmtArgs {
    target: FmtTarget,
    /// `--debug`: dump the token stream of each input before formatting it.
    debug: bool,
}

/// Hand-written because `FmtArgs` holds [`FmtTarget`] rather than one bool per
/// flag, which no `#[derive(Args)]` spelling produces. The `Command` this
/// builds is what `--help` and `scarlet man` render, so each arg keeps its
/// help text.
impl clap::FromArgMatches for FmtArgs {
    fn from_arg_matches(m: &clap::ArgMatches) -> Result<Self, clap::Error> {
        let target = if m.get_flag("stdin") {
            FmtTarget::Stdin
        } else {
            FmtTarget::Files {
                path: m.get_one::<String>("path").cloned(),
                action: if m.get_flag("check") {
                    FileAction::Check
                } else if m.get_flag("stdout") {
                    FileAction::Print
                } else {
                    FileAction::WriteBack
                },
            }
        };
        Ok(FmtArgs {
            target,
            debug: m.get_flag("debug"),
        })
    }

    fn update_from_arg_matches(&mut self, m: &clap::ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(m)?;
        Ok(())
    }
}

impl Args for FmtArgs {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        cmd.arg(clap::Arg::new("path").value_name("PATH").index(1))
            .arg(
                clap::Arg::new("stdout")
                    .long("stdout")
                    .help("Print formatted output instead of writing to files")
                    .action(clap::ArgAction::SetTrue)
                    .conflicts_with_all(["check", "stdin"]),
            )
            .arg(
                clap::Arg::new("stdin")
                    .long("stdin")
                    .help("Read input from stdin instead of a file")
                    .action(clap::ArgAction::SetTrue)
                    .conflicts_with_all(["check", "path", "stdout"]),
            )
            .arg(
                clap::Arg::new("check")
                    .long("check")
                    .help("Check if files are formatted (exit 1 if not)")
                    .action(clap::ArgAction::SetTrue),
            )
            .arg(
                clap::Arg::new("debug")
                    .long("debug")
                    .help("Print debug information about tokens")
                    .action(clap::ArgAction::SetTrue),
            )
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        Self::augment_args(cmd)
    }
}

/// Resolve a diagnostic's module provenance to (path, text) so it renders
/// against the file its span actually points into.
fn resolve_diagnostic_source(key: &scarlet::module::ModuleKey) -> Option<(PathBuf, String)> {
    let path: scarlet::module::ModulePath = key.as_str().split('/').map(str::to_string).collect();
    match scarlet::module::resolve_canonical(&path).ok()?.source {
        scarlet::module::ModuleSource::File(p) => {
            let text = fs::read_to_string(&p).ok()?;
            Some((p, text))
        }
        scarlet::module::ModuleSource::Embedded(s) => {
            Some((PathBuf::from(key.as_str()), s.to_string()))
        }
    }
}

/// Print diagnostics (if any) and exit when `fail` is set.
fn report(diagnostics: &[diagnostic::Diagnostic], fail: bool, file: &str, entrypoint: &str) {
    if !diagnostics.is_empty() {
        diagnostic::print_diagnostics(diagnostics, file, entrypoint, &resolve_diagnostic_source);
        if fail {
            process::exit(1);
        }
    }
}

fn parse_source(file: &str, entrypoint: &str) -> ast::Expression {
    let mut s = scanner::new_scanner(file.to_string());
    let p = parser::new_parser(&mut s);
    let result = p.parse_program();

    let fail = diagnostic::has_errors(&result.diagnostics);
    report(&result.diagnostics, fail, file, entrypoint);

    ast::Expression::BlockExpression(result.ast)
}

fn compile_source(
    expr: &ast::Expression,
    file: &str,
    entrypoint: &str,
    f: impl FnOnce(&ast::Expression, Option<&Path>) -> bytecode::CompileResult,
) -> bytecode::CompileResult {
    let path = Path::new(entrypoint);
    let base_dir = path.parent();
    // Editing the Scarlet repo's own stdlib: analyse as that module so `@vm` and
    // external are permitted and prelude self-redefinition is suppressed.
    let result = match scarlet::module::detect_stdlib_module(path) {
        Some(m) => bytecode::check_as_module(expr, base_dir, m),
        None => f(expr, base_dir),
    };

    report(&result.diagnostics, !result.success(), file, entrypoint);

    result
}

fn find_scarlet_files(path: &str) -> io::Result<Vec<PathBuf>> {
    let p = Path::new(path);
    if p.is_file() {
        if path.ends_with(".scrl") {
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
    scarlet::module::collect_scrl_files(p, &mut files);
    Ok(files)
}

/// Spans are 0-indexed; `fmt` output is 1-indexed for both files and stdin.
fn render_fmt_diagnostic(path: impl std::fmt::Display, d: &diagnostic::Diagnostic) -> String {
    let line = d.span.start_line + 1;
    let col = d.span.start_column + 1;
    format!("{path}:{line}:{col}: {}", d.message)
}

fn dump_tokens(src: &str) {
    let mut s = scanner::new_scanner(src.to_string());
    let (tokens, diagnostics) = s.scan_all();
    for tok in tokens {
        eprintln!("Token: {:?} trivia: {}", tok.kind, tok.leading_trivia.len());
        for t in &tok.leading_trivia {
            eprintln!("  Trivia: {t:?}");
        }
    }
    for d in &diagnostics {
        eprintln!(
            "Scan error: {}:{}: {}",
            d.span.start_line + 1,
            d.span.start_column + 1,
            d.message
        );
    }
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("{msg}");
    process::exit(1);
}

fn read_file_or_die(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| die(e))
}

fn main() -> process::ExitCode {
    // clap is the parser and command model only; `scarlet::cli` renders all
    // help/version/error/man output. The meta flags are intercepted before clap
    // so they work without satisfying required args (`scarlet run --help`).
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let cmd = Cli::command();

    if raw.iter().any(|a| a == "-V" || a == "--version") {
        help::version(&cmd);
        return process::ExitCode::SUCCESS;
    }

    let wants_help = raw.iter().any(|a| a == "-h" || a == "--help");
    let help_word = raw.first().map(String::as_str) == Some("help");
    if wants_help || help_word {
        let target = raw
            .iter()
            .find(|a| !a.starts_with('-') && a.as_str() != "help")
            .map(String::as_str);
        return help::help(&cmd, target);
    }

    if raw.is_empty() {
        help::home(&cmd);
        return process::ExitCode::SUCCESS;
    }

    if raw.first().map(String::as_str) == Some("man") {
        if let Err(e) = man::render(&cmd) {
            die(format!("could not render man page: {e}"));
        }
        return process::ExitCode::SUCCESS;
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
        Some(Commands::Dis(args)) => {
            let file = read_file_or_die(&args.entrypoint);
            let expr = parse_source(&file, &args.entrypoint);
            let result = compile_source(&expr, &file, &args.entrypoint, bytecode::compile);
            let Some(program) = result.into_runnable() else {
                die("nothing to list: the compile produced no program");
            };
            let filter = match &args.only {
                Some(name) => dis::Filter::Named(name),
                None => dis::Filter::Entry,
            };
            match dis::listing(&program, filter) {
                Some(text) => print!("{text}"),
                None => die(format!(
                    "no function matching '{}'",
                    args.only.as_deref().unwrap_or_default()
                )),
            }
        }
        Some(Commands::Check { entrypoint }) => {
            let file = read_file_or_die(&entrypoint);
            let expr = parse_source(&file, &entrypoint);
            compile_source(&expr, &file, &entrypoint, bytecode::check);
        }
        Some(Commands::Build { entrypoint }) => {
            eprintln!("warning: `al build` is deprecated; use `al fmt --stdout <file>`");
            cmd_fmt(FmtArgs {
                target: FmtTarget::Files {
                    path: Some(entrypoint),
                    action: FileAction::Print,
                },
                debug: false,
            });
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
        Some(Commands::Lint(args)) => {
            cmd_lint(args);
        }
    }
    process::ExitCode::SUCCESS
}

/// The `.scrl` illegal-state census. It prints what the corpus contains and
/// exits 0 either way — `scarlet_core::lint`'s module doc carries the
/// measurement behind this being a command you run rather than a warning the
/// compiler emits by default.
fn cmd_lint(args: LintArgs) {
    let roots = if args.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        args.paths
    };
    let census = lint::run(&roots, args.min_bools);
    print!("{}", lint::report(&census, args.min_bools));
}

fn cmd_run(args: RunArgs) {
    let file = read_file_or_die(&args.entrypoint);
    let expr = parse_source(&file, &args.entrypoint);

    if args.debug_printer {
        println!();
        println!("================DEBUG: Printed parsed source code================");
        if let formatter::FormatResult::Formatted { output } = formatter::format(&file) {
            println!("{output}");
        }
        println!("=================================================================");
        println!();
    }

    let result = compile_source(&expr, &file, &args.entrypoint, bytecode::compile);
    let Some(program) = result.into_runnable() else {
        die("nothing to run: the compile produced no program");
    };
    let mut out = io::BufWriter::new(io::stdout().lock());
    let outcome = scarlet_vm::run(&program, &mut out);
    // Flushed before any message, so the program's own output comes first.
    let flushed = out.flush();
    match outcome {
        Ok(()) if flushed.is_ok() => {}
        // A closed pipe, like `| head`, is the reader leaving, not a failure.
        Ok(()) | Err(scarlet_vm::Stop::OutputClosed) => {}
        Err(scarlet_vm::Stop::NotBuiltYet(what)) => {
            die(format!("cannot run: the new VM does not run {what} yet"))
        }
        Err(scarlet_vm::Stop::HeapFull) => die("the program ran out of heap"),
        Err(scarlet_vm::Stop::BadProgram(what)) => die(format!(
            "internal error: {what}. This is a bug in the compiler, not in the program"
        )),
    }
}

/// `al fmt --stdin`: format stdin and print the result. Separate from the file
/// walk because there is no file to write back to, check, or name in an error.
fn fmt_stdin(debug: bool) {
    let mut content = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut content) {
        die(format!("Error reading stdin: {e}"));
    }
    if debug {
        dump_tokens(&content);
    }
    match formatter::format(&content) {
        formatter::FormatResult::Formatted { output } => {
            print!("{output}");
            let _ = io::stdout().flush();
        }
        formatter::FormatResult::ParseFailed { errors } => {
            for d in &errors {
                eprintln!("{}", render_fmt_diagnostic("stdin", d));
            }
            process::exit(1);
        }
        formatter::FormatResult::CommentsLost { comment } => {
            // Pass the input through untouched so no comment is deleted.
            eprintln!(
                "formatter bug: formatting would delete the comment `{comment}`; input left unchanged"
            );
            print!("{content}");
            let _ = io::stdout().flush();
            process::exit(1);
        }
        formatter::FormatResult::OutputInvalid { detail } => {
            // Pass the input through so valid source is never replaced.
            eprintln!("formatter bug: {detail}; input left unchanged");
            print!("{content}");
            let _ = io::stdout().flush();
            process::exit(1);
        }
    }
}

fn cmd_fmt(args: FmtArgs) {
    let (path, action) = match args.target {
        FmtTarget::Stdin => return fmt_stdin(args.debug),
        FmtTarget::Files { path, action } => (path, action),
    };

    let path = path.as_deref().unwrap_or(".");

    let files = find_scarlet_files(path).unwrap_or_else(|e| die(e));

    if files.is_empty() {
        println!("No .scrl files found");
        return;
    }

    let mut needs_formatting = false;
    let mut has_errors = false;

    for file in &files {
        let content = match fs::read_to_string(file) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Error formatting {}: {e}", file.display());
                has_errors = true;
                continue;
            }
        };

        if args.debug {
            dump_tokens(&content);
        }

        match formatter::format(&content) {
            formatter::FormatResult::ParseFailed { errors } => {
                for d in &errors {
                    eprintln!("{}", render_fmt_diagnostic(file.display(), d));
                }
                has_errors = true;
            }
            formatter::FormatResult::CommentsLost { comment } => {
                eprintln!(
                    "Error formatting {}: formatter bug: formatting would delete the comment \
                     `{comment}`; file left unchanged",
                    file.display()
                );
                has_errors = true;
            }
            formatter::FormatResult::OutputInvalid { detail } => {
                eprintln!(
                    "Error formatting {}: formatter bug: {detail}; file left unchanged",
                    file.display()
                );
                has_errors = true;
            }
            formatter::FormatResult::Formatted { output } => {
                let changed = output != content;
                match action {
                    FileAction::Check => {
                        if changed {
                            println!("{} needs formatting", file.display());
                            needs_formatting = true;
                        }
                    }
                    FileAction::Print => print!("{output}"),
                    FileAction::WriteBack => {
                        if changed {
                            if let Err(e) = fs::write(file, &output) {
                                eprintln!("Error writing {}: {e}", file.display());
                                has_errors = true;
                                continue;
                            }
                            println!("Formatted {}", file.display());
                        }
                    }
                }
            }
        }
    }

    if has_errors {
        process::exit(1);
    }

    if matches!(action, FileAction::Check) && needs_formatting {
        process::exit(1);
    }
}

/// Why `al upgrade` failed. Rendered once, by `die`.
enum UpgradeError {
    UnsupportedOs,
    LocateExe(std::io::Error),
    Download {
        url: String,
        source: Box<ureq::Error>,
    },
    Read(std::io::Error),
    Io {
        action: &'static str,
        path: std::path::PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for UpgradeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpgradeError::UnsupportedOs => write!(f, "unsupported OS"),
            UpgradeError::LocateExe(e) => write!(f, "cannot locate current executable: {e}"),
            UpgradeError::Download { url, source } => {
                write!(f, "download of {url} failed: {source}")
            }
            UpgradeError::Read(e) => write!(f, "read error: {e}"),
            UpgradeError::Io {
                action,
                path,
                source,
            } => write!(f, "{action} {}: {source}", path.display()),
        }
    }
}

fn cmd_upgrade(version: Option<String>) -> Result<(), UpgradeError> {
    let current_exe = std::env::current_exe().map_err(UpgradeError::LocateExe)?;

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
        return Err(UpgradeError::UnsupportedOs);
    };

    let asset_name = format!("scarlet-{os_name}-{arch}");
    // Download next to the target so the rename is same-device; temp_dir is
    // often tmpfs on Linux, which fails with EXDEV.
    let tmp_path = current_exe.with_extension("new");
    let download_url = format!(
        "https://github.com/scarletindustries/language/releases/download/{tag}/{asset_name}"
    );

    println!("Downloading {tag}...");

    let resp = ureq::get(&download_url)
        .call()
        .map_err(|e| UpgradeError::Download {
            url: download_url.clone(),
            source: Box::new(e),
        })?;
    let len: u64 = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut reader = resp.into_reader();
    let mut out = fs::File::create(&tmp_path).map_err(|e| UpgradeError::Io {
        action: "cannot create",
        path: tmp_path.clone(),
        source: e,
    })?;
    let mut buf = [0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = reader.read(&mut buf).map_err(UpgradeError::Read)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| UpgradeError::Io {
            action: "cannot write",
            path: tmp_path.clone(),
            source: e,
        })?;
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
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755)).map_err(|e| {
            UpgradeError::Io {
                action: "cannot set permissions on",
                path: tmp_path.clone(),
                source: e,
            }
        })?;
    }

    if let Err(e) = fs::rename(&tmp_path, &current_exe) {
        let _ = fs::remove_file(&tmp_path);
        return Err(UpgradeError::Io {
            action: "cannot replace",
            path: current_exe.clone(),
            source: e,
        });
    }

    match process::Command::new(&current_exe)
        .arg("--version")
        .output()
    {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout);
            let v = v.trim().strip_prefix("al ").unwrap_or(v.trim());
            println!("Upgraded to {v}");
        }
        _ => println!("Upgrade complete"),
    }
    Ok(())
}

#[inline]
fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}
