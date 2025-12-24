module main

import os
import cli
import net.http
import downloader
import ast
import scanner
import parser
import printer
import formatter
import bytecode
import vm
import diagnostic
import types
import repl

const version = $embed_file('../VERSION').to_string().trim_space()

struct ParsedSource {
	ast         ast.BlockExpression
	diagnostics []diagnostic.Diagnostic
}

fn parse_source(file string, entrypoint string) !ParsedSource {
	mut s := scanner.new_scanner(file)
	mut p := parser.new_parser(mut s)
	result := p.parse_program()

	if result.diagnostics.len > 0 {
		diagnostic.print_diagnostics(result.diagnostics, file, entrypoint)
		if diagnostic.has_errors(result.diagnostics) {
			exit(1)
		}
	}

	return ParsedSource{
		ast:         result.ast
		diagnostics: result.diagnostics
	}
}

fn check_source(program ast.BlockExpression, file string, entrypoint string) !types.CheckResult {
	result := types.check(program)

	if result.diagnostics.len > 0 {
		diagnostic.print_diagnostics(result.diagnostics, file, entrypoint)
		if !result.success {
			exit(1)
		}
	}

	return result
}

fn find_al_files(path string) ![]string {
	if os.is_file(path) {
		if path.ends_with('.al') {
			return [path]
		}
		return []
	}

	if !os.is_dir(path) {
		return error('Path does not exist: ${path}')
	}

	mut files := []string{}
	entries := os.walk_ext(path, '.al')
	for entry in entries {
		files << entry
	}
	return files
}

struct FormatFileResult {
	changed    bool
	output     string
	has_errors bool
	errors     []string
}

fn format_file(path string, debug bool) !FormatFileResult {
	content := os.read_file(path)!
	result := formatter.format_with_debug(content, debug)

	if result.has_errors {
		mut error_msgs := []string{}
		for d in result.diagnostics {
			error_msgs << '${path}:${d.span.start_line}:${d.span.start_column}: ${d.message}'
		}
		return FormatFileResult{
			changed:    false
			output:     content
			has_errors: true
			errors:     error_msgs
		}
	}

	return FormatFileResult{
		changed:    result.output != content
		output:     result.output
		has_errors: false
		errors:     []
	}
}

fn main() {
	mut app := cli.Command{
		name:        'al'
		description: 'A small, expressive programming language'
		version:     version
		posix_mode:  true
		execute:     fn (cmd cli.Command) ! {
			println('
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
')
		}
		commands:    [
			cli.Command{
				name:        'repl'
				description: 'Start an interactive REPL session'
				execute:     fn (cmd cli.Command) ! {
					repl.run(version)
				}
			},
			cli.Command{
				name:          'check'
				required_args: 1
				usage:         '<entrypoint>'
				description:   'Type check a program without running it'
				execute:       fn (cmd cli.Command) ! {
					entrypoint := cmd.args[0]
					file := os.read_file(entrypoint)!
					parsed := parse_source(file, entrypoint)!
					check_source(parsed.ast, file, entrypoint)!
				}
			},
			cli.Command{
				name:          'build'
				required_args: 1
				usage:         '<entrypoint>'
				description:   'Parse and print the AST of a program'
				execute:       fn (cmd cli.Command) ! {
					entrypoint := cmd.args[0]
					file := os.read_file(entrypoint)!
					parsed := parse_source(file, entrypoint)!
					println(printer.print_expr(parsed.ast))
				}
			},
			cli.Command{
				name:        'fmt'
				usage:       '[path]'
				description: 'Format AL source files'
				flags:       [
					cli.Flag{
						flag:        .bool
						name:        'stdout'
						description: 'Print formatted output instead of writing to files'
					},
					cli.Flag{
						flag:        .bool
						name:        'stdin'
						description: 'Read input from stdin instead of a file'
					},
					cli.Flag{
						flag:        .bool
						name:        'check'
						description: 'Check if files are formatted (exit 1 if not)'
					},
					cli.Flag{
						flag:        .bool
						name:        'debug'
						description: 'Print debug information about tokens'
					},
				]
				execute:     fn (cmd cli.Command) ! {
					from_stdin := cmd.flags.get_bool('stdin')!
					debug := cmd.flags.get_bool('debug')!

					if from_stdin {
						mut content := ''
						for {
							line := os.get_raw_line()
							if line.len == 0 {
								break
							}
							content += line
						}
						result := formatter.format_with_debug(content, debug)
						if result.has_errors {
							for d in result.diagnostics {
								eprintln('stdin:${d.span.start_line}:${d.span.start_column}: ${d.message}')
							}
							exit(1)
						}
						print(result.output)
						return
					}

					path := if cmd.args.len > 0 { cmd.args[0] } else { '.' }
					to_stdout := cmd.flags.get_bool('stdout')!
					check_only := cmd.flags.get_bool('check')!

					files := find_al_files(path)!

					if files.len == 0 {
						println('No .al files found')
						return
					}

					mut needs_formatting := false
					mut has_errors := false

					for file in files {
						result := format_file(file, debug) or {
							eprintln('Error formatting ${file}: ${err}')
							has_errors = true
							continue
						}

						if result.has_errors {
							for err_msg in result.errors {
								eprintln(err_msg)
							}
							has_errors = true
							continue
						}

						if check_only {
							if result.changed {
								println('${file} needs formatting')
								needs_formatting = true
							}
						} else if to_stdout {
							print(result.output)
						} else {
							if result.changed {
								os.write_file(file, result.output)!
								println('Formatted ${file}')
							}
						}
					}

					if has_errors {
						exit(1)
					}

					if check_only && needs_formatting {
						exit(1)
					}
				}
			},
			cli.Command{
				name:        'upgrade'
				usage:       '[version]'
				description: 'Upgrade to a specific version (default: canary)'
				execute:     fn (cmd cli.Command) ! {
					current_exe := os.executable()

					tag := if cmd.args.len > 0 {
						v := cmd.args[0]
						if v == 'canary' || v.contains('canary') {
							v
						} else if v[0].is_digit() {
							'v${v}'
						} else {
							v
						}
					} else {
						'canary'
					}

					arch := $if arm64 {
						'arm64'
					} $else {
						'x86_64'
					}

					os_name := $if macos {
						'macos'
					} $else $if linux {
						'linux'
					} $else {
						return error('Unsupported OS')
					}

					asset_name := 'al-${os_name}-${arch}'
					tmp_dir := os.temp_dir()
					tmp_path := os.join_path(tmp_dir, asset_name)
					download_url := 'https://github.com/alii/al/releases/download/${tag}/${asset_name}'

					println('Downloading ${tag}...')

					mut dl := downloader.ProgressDownloader{}
					http.download_file_with_progress(download_url, tmp_path, downloader: &dl) or {
						return error('Failed to download: ${err}')
					}

					os.chmod(tmp_path, 0o755)!
					os.mv(tmp_path, current_exe)!

					new_version := os.execute('${current_exe} --version')
					if new_version.exit_code == 0 {
						println('Upgraded to ${new_version.output.trim_space().replace('al version ',
							'')}')
					} else {
						println('Upgraded successfully!')
					}
				}
			},
			cli.Command{
				name:          'run'
				required_args: 1
				usage:         '<entrypoint>'
				description:   'Run a program'
				flags:         [
					cli.Flag{
						flag:        .bool
						name:        'debug-printer'
						description: 'Print the parsed program before execution starts'
					},
					cli.Flag{
						flag:        .bool
						name:        'expose-debug-builtins'
						description: 'Expose debug builtins like __stack_depth__()'
					},
					cli.Flag{
						flag:        .bool
						name:        'experimental-shitty-io'
						description: 'Enable experimental blocking I/O (file and network)'
					},
					cli.Flag{
						flag:        .bool
						name:        'experimental-std-lib'
						description: 'Enable experimental standard library functions'
					},
				]
				execute:       fn (cmd cli.Command) ! {
					entrypoint := cmd.args[0]
					debug_printer := cmd.flags.get_bool('debug-printer')!
					expose_debug_builtins := cmd.flags.get_bool('expose-debug-builtins')!
					io_enabled := cmd.flags.get_bool('experimental-shitty-io')!
					std_lib_enabled := cmd.flags.get_bool('experimental-std-lib')!

					file := os.read_file(entrypoint)!
					parsed := parse_source(file, entrypoint)!
					checked := check_source(parsed.ast, file, entrypoint)!

					if debug_printer {
						println('')
						println('================DEBUG: Printed parsed source code================')
						println(printer.print_expr(parsed.ast))
						println('=================================================================')
						println('')
					}

					program := bytecode.compile(checked.typed_ast, checked.env,
						expose_debug_builtins: expose_debug_builtins
					)!

					mut v := vm.new_vm(program, io_enabled: io_enabled, std_lib_enabled: std_lib_enabled)
					run_result := v.run()!

					if run_result !is bytecode.NoneValue {
						println(vm.inspect(run_result))
					}
				}
			},
		]
	}

	app.setup()

	app.parse(os.args)
}
