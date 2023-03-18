module main

import os
import cli
import lib.compiler.scanner

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

					mut s := scanner.new_scanner(os.read_file(entrypoint)!)

					for {
						t := s.scan_next()

						if t.kind == .eof {
							break
						}

						println(t)
					}
				}
			},
		]
	}

	app.setup()
	app.parse(os.args)
}
