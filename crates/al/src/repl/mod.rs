use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::ast;
use crate::bytecode;
use crate::diagnostic::{self, DiagnosticCode};
use crate::parser;
use crate::scanner;
use crate::span::point_span;
use crate::vm;

pub fn run(version: &str) {
    println!("al {} REPL", version);
    println!("Type expressions to evaluate. Use 'exit' or Ctrl+D to quit.");
    println!();

    let mut input_buffer = String::new();
    let mut continuation = false;
    let mut definitions: Vec<ast::Node> = Vec::new();
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

        match eval_input(&input_buffer, &definitions) {
            EvalOutcome::Incomplete => {
                continuation = true;
            }
            EvalOutcome::Failed => {
                input_buffer.clear();
                continuation = false;
            }
            EvalOutcome::Ok(new_defs) => {
                definitions.extend(new_defs);
                input_buffer.clear();
                continuation = false;
            }
        }
    }
}

enum EvalOutcome {
    Incomplete,
    Failed,
    Ok(Vec<ast::Node>),
}

fn eval_input(input: &str, definitions: &[ast::Node]) -> EvalOutcome {
    let mut input_scanner = scanner::new_scanner(input.to_string());
    let mut input_parser = parser::new_parser(&mut input_scanner);
    let input_parse_result = input_parser.parse_program();

    if diagnostic::has_errors(&input_parse_result.diagnostics) {
        if input_parse_result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagnosticCode::UnexpectedEof)
        {
            return EvalOutcome::Incomplete;
        }
        diagnostic::print_diagnostics(&input_parse_result.diagnostics, input, "<repl>");
        return EvalOutcome::Failed;
    }

    let mut combined_body: Vec<ast::Node> = Vec::new();
    combined_body.extend(definitions.iter().cloned());
    combined_body.extend(input_parse_result.ast.body.iter().cloned());

    let combined_ast = ast::Expression::BlockExpression(ast::BlockExpression {
        body: combined_body,
        span: point_span(1, 1),
    });

    let result = bytecode::compile(&combined_ast, None, Some(crate::stdlib()));

    if !result.diagnostics.is_empty() {
        diagnostic::print_diagnostics(&result.diagnostics, input, "<repl>");
        if !result.success {
            return EvalOutcome::Failed;
        }
    }

    let program = result.program;

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

    let mut new_definitions: Vec<ast::Node> = Vec::new();
    for node in &input_parse_result.ast.body {
        if matches!(node, ast::Node::Statement(_)) {
            new_definitions.push(node.clone());
        }
    }

    EvalOutcome::Ok(new_definitions)
}
