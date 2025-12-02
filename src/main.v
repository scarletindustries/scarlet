module main

import os
import cli
import compiler.scanner
import compiler.parser
import compiler.printer
import compiler.bytecode
import compiler.vm

fn main() {
	mut app := cli.Command{
		name:        'al'
		description: 'al compiler and toolchain'
		version:     '0.0.1'
		posix_mode:  true
		execute:     fn (cmd cli.Command) ! {
			println(cmd.help_message())
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
				name:          'compile'
				required_args: 2
				usage:         '<entrypoint> <output.alb>'
				description:   'Compile a program to bytecode (.alb file)'
				execute:       fn (cmd cli.Command) ! {
					entrypoint := cmd.args[0]
					output := cmd.args[1]

					// Validate output extension
					if !output.ends_with('.alb') {
						return error('Output file must have .alb extension')
					}

					file := os.read_file(entrypoint)!

					mut s := scanner.new_scanner(file)
					mut p := parser.new_parser(mut s)

					ast := p.parse_program()!
					program := bytecode.compile(ast)!

					// Serialize and write to file
					data := program.serialize()
					os.write_file_array(output, data)!

					println('Compiled ${entrypoint} -> ${output} (${data.len} bytes)')
				}
			},
			cli.Command{
				name:          'run'
				required_args: 1
				usage:         '<entrypoint>'
				description:   'Run a program (.al source or .alb bytecode)'
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

					program := if entrypoint.ends_with('.alb') {
						if debug_printer {
							return error('Cannot run compiled bytecode with `--debug-printer`')
						}

						data := os.read_bytes(entrypoint)!
						bytecode.deserialize(data)!
					} else {
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

						bytecode.compile(ast)!
					}

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
