module main

import os
import cli
import compiler.scanner
import compiler.parser
import compiler.printer
import compiler.bytecode
import compiler.vm

const version = $embed_file('../VERSION').to_string().trim_space()

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

   Examples:
     al run hello.al
     al run examples/fibonacci.al

   Learn more: https://al.alistair.sh
')
		}
		commands:    [
			cli.Command{
				name:          'build'
				required_args: 1
				usage:         '<entrypoint>'
				description:   'Parse and print the AST of a program'
				execute:       fn (cmd cli.Command) ! {
					entrypoint := cmd.args[0]
					file := os.read_file(entrypoint)!

					mut s := scanner.new_scanner(file)
					mut p := parser.new_parser(mut s)

					ast := p.parse_program()!

					println(printer.print_expr(ast))
				}
			},
			cli.Command{
				name:        'upgrade'
				description: 'Upgrade to the latest version'
				execute:     fn (cmd cli.Command) ! {
					current_exe := os.executable()

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

					println('Downloading latest version...')

					result := os.execute('gh release download canary --repo alii/al --pattern "${asset_name}" --dir "${tmp_dir}" --clobber')
					if result.exit_code != 0 {
						return error('Failed to download: ${result.output}')
					}

					os.chmod(tmp_path, 0o755)!
					os.mv(tmp_path, current_exe)!

					println('Upgraded successfully!')
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
				]
				execute:       fn (cmd cli.Command) ! {
					entrypoint := cmd.args[0]
					debug_printer := cmd.flags.get_bool('debug-printer')!

					file := os.read_file(entrypoint)!

					mut s := scanner.new_scanner(file)
					mut p := parser.new_parser(mut s)

					ast := p.parse_program()!

					if debug_printer {
						println('')
						println('================DEBUG: Printed parsed source code================')
						println(printer.print_expr(ast))
						println('=================================================================')
						println('')
					}

					program := bytecode.compile(ast)!

					mut v := vm.new_vm(program)
					result := v.run()!

					if result !is bytecode.NoneValue {
						println(vm.inspect(result))
					}
				}
			},
		]
	}

	app.setup()

	app.parse(os.args)
}
