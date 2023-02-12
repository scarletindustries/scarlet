module main

import os
import cli
import lib.compiler

fn main() {
	mut app := cli.Command{
		description: 'al compiler and toolchain'
		version: '1.0.0'
		posix_mode: true
		execute: fn (cmd cli.Command) ! {
			println(cmd.help_message())
		}
		commands: [
			cli.Command{
				name: 'build'
				execute: fn (cmd cli.Command) ! {
					program_path := '/Users/ali/code/al/program/src/main.al'

					mut scanner := compiler.new_scanner(program_path, os.read_file(program_path)!)

					scanner.scan()

					println(scanner.all_tokens)
				}
			},
		]
	}

	app.setup()
	app.parse(os.args)
}
