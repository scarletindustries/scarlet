use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::ast;
use crate::bytecode;
use crate::diagnostic::{self, DiagnosticCode};
use crate::parser;
use crate::scanner;
use crate::vm;

pub fn run(version: &str) {
    println!("al {} REPL", version);
    println!("Type expressions to evaluate. Use 'exit' or Ctrl+D to quit.");
    println!();

    let mut input_buffer = String::new();
    let mut continuation = false;
    // Source of every entry that defined something, replayed verbatim ahead of
    // the current one. Source, not AST: see `eval_input`.
    let mut prelude = String::new();
    let mut rl = match DefaultEditor::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Failed to initialize readline: {}", e);
            return;
        }
    };

    loop {
        let prompt = if continuation { "... " } else { ">>> " };

        let line = match rl.readline(prompt) {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => {
                // Ctrl-C: abandon the current (possibly multi-line) entry and
                // return to a fresh prompt without exiting.
                println!("^C");
                input_buffer.clear();
                continuation = false;
                continue;
            }
            Err(ReadlineError::Eof) => {
                println!();
                break;
            }
            Err(e) => {
                eprintln!("readline: {e}");
                break;
            }
        };

        let line_trimmed = line.trim_end_matches(['\r', '\n']);

        if !continuation && line_trimmed == "exit" {
            break;
        }

        if !line_trimmed.is_empty() {
            let _ = rl.add_history_entry(line_trimmed);
        } else if !continuation {
            continue;
        }

        if continuation {
            input_buffer.push('\n');
            input_buffer.push_str(line_trimmed);
        } else {
            input_buffer = line_trimmed.to_string();
        }

        if input_buffer.trim().is_empty() {
            input_buffer.clear();
            continuation = false;
            continue;
        }

        match eval_input(&input_buffer, &prelude) {
            EvalOutcome::Incomplete => {
                continuation = true;
            }
            EvalOutcome::Failed => {
                input_buffer.clear();
                continuation = false;
            }
            EvalOutcome::Ok(replay) => {
                if let Some(src) = replay {
                    prelude.push_str(&src);
                }
                input_buffer.clear();
                continuation = false;
            }
        }
    }
}

enum EvalOutcome {
    Incomplete,
    Failed,
    /// The entry's source, iff it defined something worth replaying.
    Ok(Option<String>),
}

/// Compile `prelude ++ input` as one source text and run it.
///
/// The prelude is *source*, not a `Vec<ast::Node>` cloned from earlier turns.
/// Each turn is scanned on its own, so every retained entry's spans would
/// restart at line 1 and two same-shaped lines would occupy the identical
/// `Span` — and the compiler keys its expression types on `Span`. Replaying
/// text instead makes every span unique by construction, and points
/// diagnostics at the entry that actually produced them.
fn eval_input(input: &str, prelude: &str) -> EvalOutcome {
    // Parse the entry alone first: only its diagnostics belong to the user,
    // and an unterminated form must ask for another line rather than replay.
    let mut probe_scanner = scanner::new_scanner(input.to_string());
    let probe_parser = parser::new_parser(&mut probe_scanner);
    let probe = probe_parser.parse_program();

    if diagnostic::has_errors(&probe.diagnostics) {
        if probe
            .diagnostics
            .iter()
            .any(|d| d.code == DiagnosticCode::UnexpectedEof)
        {
            return EvalOutcome::Incomplete;
        }
        diagnostic::print_diagnostics(&probe.diagnostics, input, "<repl>", &|_| None);
        return EvalOutcome::Failed;
    }

    let combined_src = format!("{prelude}{input}\n");
    let mut combined_scanner = scanner::new_scanner(combined_src.clone());
    let combined_parser = parser::new_parser(&mut combined_scanner);
    let combined = combined_parser.parse_program();
    if diagnostic::has_errors(&combined.diagnostics) {
        diagnostic::print_diagnostics(&combined.diagnostics, &combined_src, "<repl>", &|_| None);
        return EvalOutcome::Failed;
    }
    let combined_ast = ast::Expression::BlockExpression(combined.ast);

    let result = bytecode::compile(&combined_ast, None, Some(&crate::STDLIB));

    if !result.diagnostics.is_empty() {
        diagnostic::print_diagnostics(&result.diagnostics, &combined_src, "<repl>", &|_| None);
        if !result.success() {
            return EvalOutcome::Failed;
        }
    }

    let Some(emitted) = result.emitted else {
        // A successful non-check compile always emits; reaching here means
        // the stdlib seed itself failed, which the diagnostics above covered.
        return EvalOutcome::Failed;
    };
    let program = emitted.program;

    let mut v = match vm::new_vm(program) {
        Ok(vm) => vm,
        Err(err) => {
            eprintln!("Runtime error: {}", err);
            return EvalOutcome::Failed;
        }
    };
    let run_result = match v.run() {
        Ok(val) => val,
        Err(err) => {
            // Per `VM::run`'s contract, an errored run leaks its scheduler
            // runtime (worker and blocking-pool threads); the REPL accepts
            // one such leak per errored evaluation to keep the session
            // alive. A worker-side error still exits the whole process.
            eprintln!("Runtime error: {}", err);
            return EvalOutcome::Failed;
        }
    };

    println!("{}", vm::inspect(&run_result, v.program()));

    // Replay an entry only if it bound or declared something; a bare
    // expression is a value the user already saw, and re-running it every
    // turn would repeat its effects.
    let defines = probe
        .ast
        .body
        .iter()
        .any(|n| matches!(n, ast::Node::Statement(_)));
    EvalOutcome::Ok(defines.then(|| format!("{input}\n")))
}
