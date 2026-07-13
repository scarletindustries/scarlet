// The effectful stdlib, pinned to the parts of it that are deterministic:
// al/io (files), al/time (a monotonic clock) and al/process (argv/env).
//
// Nothing here may print a value the environment chooses — a raw Instant, an
// elapsed millisecond count, argv[0], an env var's value — so every assertion
// below is a derived invariant instead. The paths are absolute: the test
// harness runs `al` from a different cwd than a human at the repo root does,
// so a relative path would not be a fixed input.

import al/array
import al/io.{IsADirectory, NotFound, UnalignedBinary}
import al/map
import al/option
import al/process
import al/scheduler
import al/string
import al/time

const PATH = '/tmp/al-effects-golden.txt'
const SNAPSHOT = '/tmp/al-effects-golden.bin'
const MISSING = '/nonexistent/al/missing.txt'

// --- al/io -----------------------------------------------------------------
// Every operation returns a Result whose error is the typed IoError sum, so a
// caller pattern-matches the failure instead of decoding an errno.

println('write: ${io.write_text(PATH, 'ledger v1')}')
println('read back: ${io.read_text(PATH) or '<failed>'}')
println('round trip: ${io.read_text(PATH) == Ok('ledger v1')}')

// Each failure lands on its own variant, carrying the path it concerns.
match io.read_text(MISSING) {
	Ok(_) -> println('missing: read a file that is not there')
	Err(NotFound(path)) -> println('missing: NotFound(${path})')
	Err(e) -> println('missing: ${e}')
}

match io.read_file('/tmp') {
	Ok(_) -> println('directory: read a directory as a file')
	Err(IsADirectory(path)) -> println('directory: IsADirectory(${path})')
	Err(e) -> println('directory: ${e}')
}

// A file is bytes, not text: write_file takes a Binary and read_file hands one
// back, so a frame written to disk comes apart under the same binary pattern
// that would read it off a socket.
println('write bytes: ${io.write_file(SNAPSHOT, <<'ALSNAP', 1:8, 2:16>>)}')
match io.read_file(SNAPSHOT) {
	Ok(<<'ALSNAP', version:8, count:16>>) -> println('snapshot: v${version}, ${count} samples')
	Ok(b) -> println('snapshot: unrecognized ${string.inspect(b)}')
	Err(e) -> println('snapshot: ${e}')
}

// A binary that is not a whole number of bytes has no on-disk form. The write
// is refused before it opens the file, so the snapshot above still stands.
match io.write_file(SNAPSHOT, <<1:4>>) {
	Ok(Nil) -> println('unaligned: wrote a half byte')
	Err(UnalignedBinary) -> println('unaligned: UnalignedBinary')
	Err(e) -> println('unaligned: ${e}')
}
println('snapshot survived: ${io.read_file(SNAPSHOT) == Ok(<<'ALSNAP', 1:8, 2:16>>)}')

// --- al/time ---------------------------------------------------------------
// An Instant is opaque and its numeric value is nobody's business; what the
// module promises is arithmetic on it, and a clock that never runs backwards.

start = time.monotonic()
deadline = time.add_ms(start, 250)
println('add_ms then since_ms: ${time.since_ms(deadline, start)}')
println('since_ms is signed: ${time.since_ms(start, deadline)}')
println('monotonic never rewinds: ${time.since_ms(time.monotonic(), start) >= 0}')

// to_deadline_ms is the escape hatch to raw monotonic ms for a @vm boundary
// that wants a number; the number itself is the machine's, only the difference
// between two of them is ours to print.
println('deadline in raw ms: ${time.to_deadline_ms(deadline) - time.to_deadline_ms(start)}')

scheduler.sleep(50)
println('slept at least 50ms: ${time.since_ms(time.monotonic(), start) >= 50}')

// --- al/process ------------------------------------------------------------
// argv[0] is the script's path and env values are the machine's, so what gets
// printed is derived from them, never the values themselves.

println('argv is non-empty: ${array.length(process.argv()) > 0}')
println('PATH is set: ${option.is_some(process.get_env('PATH'))}')
println('unset var: ${process.get_env('AL_DEFINITELY_UNSET')}')
println('env has PATH: ${map.has(process.env(), 'PATH')}')
println('env inspects as: ${string.inspect(process.env())}')
