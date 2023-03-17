module main

import os
import cli
import lib.compiler

fn main() {
	mut app := cli.Command{
		description: 'al compiler and toolchain'
		version: '0.0.1'
		disable_version: true
		posix_mode: true
		execute: fn (cmd cli.Command) ! {
			println(cmd.help_message())
		}
		commands: [
			cli.Command{
				name: 'build'
				required_args: 1
				args: ['entrypoint']
				description: 'Build and compile an entrypoint to your program'
				execute: fn (cmd cli.Command) ! {
					entrypoint := cmd.args[0]

					mut scanner := compiler.new_scanner(os.read_file(entrypoint)!)

					for {
						t := scanner.scan_next()
						println(t)

						if t.kind == .eof {
							break
						}
					}
				}
			},
		]
	}

	app.setup()
	app.parse(os.args)
}
