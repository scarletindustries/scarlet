use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process;

use clap::{Args, Parser, Subcommand};

use al::flags::Flags;
use al::{ast, bytecode, diagnostic, formatter, lsp, parser, printer, repl, scanner, vm};

const VERSION: &str = include_str!("../VERSION");

const BANNER: &str = "
   ▄▀█ █░░
   █▀█ █▄▄

   Usage:
     al run <file.al>      Run a program
     al repl               Start interactive REPL
     al --help             Show all commands

   Example:
     al run hello.al
     al check my_app.al

   Learn more: https://al.alistair.sh
";

#[derive(Parser)]
#[command(
    name = "al",
    version = VERSION.trim(),
    about = "A small, expressive programming language"
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
    /// Expose debug builtins like __stack_depth__()
    #[arg(long = "expose-debug-builtins")]
    expose_debug_builtins: bool,
    /// Enable experimental blocking I/O (file and network)
    #[arg(long = "experimental-shitty-io")]
    experimental_shitty_io: bool,
    /// Enable experimental standard library functions
    #[arg(long = "experimental-std-lib")]
    experimental_std_lib: bool,
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

struct ParsedSource {
    ast: ast::BlockExpression,
    #[allow(dead_code)]
    diagnostics: Vec<diagnostic::Diagnostic>,
}

fn parse_source(file: &str, entrypoint: &str) -> ParsedSource {
    let mut s = scanner::new_scanner(file.to_string());
    let mut p = parser::new_parser(&mut s);
    let result = p.parse_program();

    if !result.diagnostics.is_empty() {
        diagnostic::print_diagnostics(&result.diagnostics, file, entrypoint);
        if diagnostic::has_errors(&result.diagnostics) {
            process::exit(1);
        }
    }

    ParsedSource {
        ast: result.ast,
        diagnostics: result.diagnostics,
    }
}

fn check_source(
    program: ast::BlockExpression,
    file: &str,
    entrypoint: &str,
) -> bytecode::CompileResult {
    let expr = ast::Expression::BlockExpression(program);
    let result = bytecode::check(&expr, Flags::default());

    if !result.diagnostics.is_empty() {
        diagnostic::print_diagnostics(&result.diagnostics, file, entrypoint);
        if !result.success {
            process::exit(1);
        }
    }

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
    walk_al(p, &mut files)?;
    Ok(files)
}

fn walk_al(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_al(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("al") {
            out.push(path);
        }
    }
    Ok(())
}

struct FormatFileResult {
    changed: bool,
    output: String,
    has_errors: bool,
    errors: Vec<String>,
}

fn format_file(path: &Path, debug: bool) -> io::Result<FormatFileResult> {
    let content = fs::read_to_string(path)?;
    let result = formatter::format_with_debug(&content, debug);

    if result.has_errors {
        let mut error_msgs = Vec::new();
        for d in &result.diagnostics {
            // BUGFIX: V's main.v uses 0-indexed line/col here but +1 on the stdin
            // path; emit 1-indexed consistently in the rust port.
            error_msgs.push(format!(
                "{}:{}:{}: {}",
                path.display(),
                d.span.start_line + 1,
                d.span.start_column + 1,
                d.message
            ));
        }
        return Ok(FormatFileResult {
            changed: false,
            output: content,
            has_errors: true,
            errors: error_msgs,
        });
    }

    Ok(FormatFileResult {
        changed: result.output != content,
        output: result.output,
        has_errors: false,
        errors: vec![],
    })
}

fn read_file_or_die(path: &str) -> String {
    match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            process::exit(1);
        }
    }
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        None => {
            println!("{BANNER}");
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
            let parsed = parse_source(&file, &entrypoint);
            check_source(parsed.ast, &file, &entrypoint);
        }
        Some(Commands::Build { entrypoint }) => {
            let file = read_file_or_die(&entrypoint);
            let parsed = parse_source(&file, &entrypoint);
            let expr = ast::Expression::BlockExpression(parsed.ast);
            println!("{}", printer::print_expr(&expr));
        }
        Some(Commands::Upgrade { version }) => {
            if let Err(e) = cmd_upgrade(version) {
                eprintln!("upgrade failed: {e}");
                process::exit(1);
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
    let fl = Flags {
        expose_debug_builtins: args.expose_debug_builtins,
        io_enabled: args.experimental_shitty_io,
        std_lib_enabled: args.experimental_std_lib,
    };

    let file = read_file_or_die(&args.entrypoint);
    let parsed = parse_source(&file, &args.entrypoint);

    if args.debug_printer {
        println!();
        println!("================DEBUG: Printed parsed source code================");
        let expr = ast::Expression::BlockExpression(parsed.ast.clone());
        println!("{}", printer::print_expr(&expr));
        println!("=================================================================");
        println!();
    }

    let expr = ast::Expression::BlockExpression(parsed.ast);
    let result = bytecode::compile(&expr, fl);
    if !result.diagnostics.is_empty() {
        diagnostic::print_diagnostics(&result.diagnostics, &file, &args.entrypoint);
        if !result.success {
            process::exit(1);
        }
    }
    let program = result.program;

    let mut v = vm::new_vm(program, fl);
    let run_result = match v.run() {
        Ok(val) => val,
        Err(e) => {
            eprintln!("{e}");
            process::exit(1);
        }
    };

    if !matches!(run_result, bytecode::Value::None) {
        println!("{}", vm::inspect(&run_result));
    }
}

fn cmd_fmt(args: FmtArgs) {
    if args.stdin {
        let mut content = String::new();
        if let Err(e) = io::stdin().read_to_string(&mut content) {
            eprintln!("Error reading stdin: {e}");
            process::exit(1);
        }
        let result = formatter::format_with_debug(&content, args.debug);
        if result.has_errors {
            for d in &result.diagnostics {
                eprintln!(
                    "stdin:{}:{}: {}",
                    d.span.start_line + 1,
                    d.span.start_column + 1,
                    d.message
                );
            }
            process::exit(1);
        }
        print!("{}", result.output);
        let _ = io::stdout().flush();
        return;
    }

    let path = args.path.as_deref().unwrap_or(".");

    let files = match find_al_files(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            process::exit(1);
        }
    };

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
