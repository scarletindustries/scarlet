module main

import os
import cli
import compiler.scanner

fn main() {
	mut app := cli.Command{
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
				description:   'Build and compile an entrypoint to your program'
				execute:       fn (cmd cli.Command) ! {
					entrypoint := cmd.args[0]
					file := os.read_file(entrypoint)!

					println(file)
				}
			},
		]
	}

	app.setup()

	app.parse(os.args)
}
