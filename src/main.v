module main

import os
import cli
import net.http
import downloader
import compiler.ast
import compiler.scanner
import compiler.parser
import compiler.printer
import compiler.bytecode
import compiler.vm
import compiler.diagnostic
import compiler.types

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
     al --help             Show all commands

   Example:
     al run hello.al
     al check my_app.al

   Learn more: https://al.alistair.sh
')
		}
		commands:    [
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
					http.download_file_with_progress(download_url, tmp_path,
						downloader: &dl) or {
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
				]
				execute:       fn (cmd cli.Command) ! {
					entrypoint := cmd.args[0]
					debug_printer := cmd.flags.get_bool('debug-printer')!
					expose_debug_builtins := cmd.flags.get_bool('expose-debug-builtins')!
					io_enabled := cmd.flags.get_bool('experimental-shitty-io')!

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

					mut v := vm.new_vm(program, io_enabled: io_enabled)
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
