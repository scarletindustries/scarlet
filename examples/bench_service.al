// A login/session HTTP service, written the way a real one would be.
//
// This is the authoritative al benchmark workload: unlike a `fib` microbench it
// spends its time where a real server does — header lookup, binary slicing,
// map lookup, enum matching, Option/Result chaining, and the allocation of a
// fresh response body per request.
//
// al gives each connection its own process with no shared mutable state and no
// message passing, so there is nowhere to keep a session table. Sessions are
// therefore stateless and *signed*: the cookie carries `<user>.<sig>` where
// `sig` is a keyed checksum over the username. Every /me request recomputes the
// checksum from scratch, which is exactly the per-request CPU a real service
// spends validating a token.
//
// Routes:
//   GET  /            plain hello (the cheap baseline path)
//   POST /login       form body `user=&password=`, sets a signed session cookie
//   GET  /me          verifies the cookie signature, returns the profile
//   POST /logout      expires the cookie
//   GET  /users/<id>  numeric path segment, map lookup, 404 on miss

import al/http.{Request, Response, Fixed, Get, Post}
import al/http/headers.{Header}
import al/http/body
import al/binary.{Dec, Hex}
import al/map
import al/option
import al/process

type User {
	id Int
	name String
	email String
	pw_hash Int
}

const USER_COUNT = 384
const DEFAULT_PORT = 8080

// ---------------------------------------------------------------------------
// Checksum
//
// al has no bitwise operators, so the usual FNV/xorshift mixing is expressed in
// arithmetic: `%` for masking, `*` and `/` for the rotate. Everything stays
// inside 32 bits, and the widest intermediate — a 32-bit accumulator times a
// 24-bit prime — is ~2^56, comfortably inside Int. This is not a MAC and makes
// no cryptographic claim; it is a real fold over real bytes, which is what the
// benchmark needs.
// ---------------------------------------------------------------------------

const M32 = 4294967296
const FNV_PRIME = 16777619
const FNV_SEED = 2166136261
// 2^19 and 2^13: the two halves of a 13-bit left rotate on a 32-bit word.
const ROT_LO = 524288
const ROT_HI = 8192

const SECRET = <<'al-bench-signing-key-v1'>>

fn rotl13(h Int) Int {
	{ h % ROT_LO } * ROT_HI + h / ROT_LO
}

fn mix(h Int, b Int) Int {
	rotl13({ h + b } * FNV_PRIME % M32)
}

fn checksum(b Binary) Int {
	fold_bytes(b, 0, binary.byte_size(b), FNV_SEED)
}

fn fold_bytes(b Binary, i Int, n Int, h Int) Int {
	if i == n {
		h
	} else {
		fold_bytes(b, i + 1, n, mix(h, binary.byte_at(b, i)))
	}
}

// The session signature: a keyed checksum, the secret folded in before the
// username so a client cannot extend a known prefix.
fn sign(user Binary) Int {
	checksum(binary.append(SECRET, user))
}

// ---------------------------------------------------------------------------
// The users table, built once at startup.
// ---------------------------------------------------------------------------

fn make_user(i Int) User {
	name = 'user${i}'
	User(
		id: i,
		name: name,
		email: '${name}@example.com',
		pw_hash: checksum(binary.from_string('pw-${i}')),
	)
}

// Two indexes over the same records: by name for /login and /me, by id for
// /users/<id>.
type Tables {
	by_name Map(Binary, User)
	by_id Map(Int, User)
}

// Both lookups go through a helper whose return type is written out. Reading
// `Option(User)` straight off `map.get` and pattern-matching a field out of it
// trips a bug in core_ir::lower (see the note at the bottom of this file); the
// annotation is also what a reader wants at each call site anyway.
fn find_by_name(t Tables, name Binary) Option(User) {
	map.get(t.by_name, name)
}

fn find_by_id(t Tables, id Int) Option(User) {
	map.get(t.by_id, id)
}

fn build_tables(i Int, n Int, t Tables) Tables {
	if i == n {
		t
	} else {
		u = make_user(i)
		build_tables(
			i + 1,
			n,
			Tables(
				by_name: map.set(t.by_name, binary.from_string(u.name), u),
				by_id: map.set(t.by_id, u.id, u),
			),
		)
	}
}

// ---------------------------------------------------------------------------
// Wire parsing: form bodies and cookies. Both are `key=value` lists over a
// separator, so both are byte scans over zero-copy slices.
// ---------------------------------------------------------------------------

const AMP = <<'&'>>
const EQ = <<'='>>
const SEMI = <<';'>>
const DOT = <<'.'>>
const SP = 32

// The value of `key` in an `application/x-www-form-urlencoded` body. No percent
// decoding: the benchmark's clients post bare tokens, and a real service would
// hand this to a decoder rather than reimplement one here.
fn form_get(b Binary, key Binary) Option(Binary) {
	scan_pairs(b, 0, binary.byte_size(b), key, AMP, False)
}

// The value of cookie `name` in a `Cookie:` header value. Cookie pairs are
// `; `-separated, so leading spaces are skipped.
fn cookie_get(b Binary, name Binary) Option(Binary) {
	scan_pairs(b, 0, binary.byte_size(b), name, SEMI, True)
}

// One scanner for both: walk `sep`-delimited `k=v` pairs, returning the first
// value whose key matches. `skip_sp` trims the OWS that follows a `;`.
fn scan_pairs(
	b Binary,
	from Int,
	n Int,
	key Binary,
	sep Binary,
	skip_sp Bool,
) Option(Binary) {
	start = if skip_sp {
		skip_spaces(b, from, n)
	} else {
		from
	}
	end = match binary.index_of(b, sep, start) {
		Some(i) -> i
		None -> n
	}
	match binary.index_of(b, EQ, start) {
		Some(e) -> if e < end && binary.slice_bytes(b, start, e - start) == key {
			Some(binary.slice_bytes(b, e + 1, end - e - 1))
		} else {
			next_pair(b, end, n, key, sep, skip_sp)
		}
		None -> None
	}
}

fn next_pair(b Binary, end Int, n Int, key Binary, sep Binary, skip_sp Bool) Option(Binary) {
	if end < n {
		scan_pairs(b, end + 1, n, key, sep, skip_sp)
	} else {
		None
	}
}

fn skip_spaces(b Binary, i Int, n Int) Int {
	if i < n && binary.byte_at(b, i) == SP {
		skip_spaces(b, i + 1, n)
	} else {
		i
	}
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

const NAME_CONTENT_TYPE = <<'Content-Type'>>
const NAME_SET_COOKIE = <<'Set-Cookie'>>
const NAME_COOKIE = <<'Cookie'>>
const VAL_JSON = <<'application/json'>>
const VAL_TEXT = <<'text/plain; charset=utf-8'>>

fn respond(code Int, content_type Binary, s String) Response {
	bin = binary.from_string(s)
	Response(
		status: code,
		headers: [Header(name: NAME_CONTENT_TYPE, value: content_type)],
		body: Fixed(binary.byte_size(bin), body.from_binary(bin)),
	)
}

fn json(code Int, s String) Response {
	respond(code, VAL_JSON, s)
}

fn unauthorized() Response {
	json(401, '{"error":"unauthorized"}')
}

fn profile_json(u User) String {
	'{"id":${u.id},"name":"${u.name}","email":"${u.email}"}'
}

// `session=<user>.<sig>` — the whole of the server's session state, carried by
// the client and re-verified on every request.
fn session_cookie(name String, sig Int) Binary {
	binary.append(
		binary.from_string('session=${name}.'),
		binary.append(binary.from_int_ascii(sig, Hex), <<'; Path=/; HttpOnly'>>),
	)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

// The signed cookie's user, or None if the cookie is absent, malformed, or the
// signature does not match a freshly computed one.
fn verify_session(req Request, t Tables) Option(User) {
	option.then(headers.get(req.headers, NAME_COOKIE), fn(raw) option.then(
		cookie_get(raw, <<'session'>>),
		fn(token) option.then(split_token(token), fn(parts) match parts {
			(user, sig_bytes) -> match binary.parse_int(sig_bytes, Hex) {
				Ok(sig) if sig == sign(user) -> find_by_name(t, user)
				else -> None
			}
		}),
	))
}

fn split_token(token Binary) Option((Binary, Binary)) {
	n = binary.byte_size(token)
	match binary.index_of(token, DOT, 0) {
		Some(i) ->
			Some((binary.slice_bytes(token, 0, i), binary.slice_bytes(token, i + 1, n - i - 1)))
		None -> None
	}
}

fn handle_login(req Request, t Tables) Response {
	match body.collect(req.body, 8192) {
		Err(_) -> json(400, '{"error":"bad body"}')
		Ok(
			raw,
		) -> match option.then(
			form_get(raw, <<'user'>>),
			fn(u) option.map(form_get(raw, <<'password'>>), fn(p) (u, p)),
		) {
			None -> json(400, '{"error":"missing user or password"}')
			Some(creds) -> match creds {
				(user, password) -> match find_by_name(t, user) {
					None -> unauthorized()
					Some(u) -> if checksum(password) == u.pw_hash {
						http.with_header(
							json(200, '{"ok":true,"user":"${u.name}"}'),
							NAME_SET_COOKIE,
							session_cookie(u.name, sign(user)),
						)
					} else {
						unauthorized()
					}
				}
			}
		}
	}
}

fn handle_me(req Request, t Tables) Response {
	match verify_session(req, t) {
		None -> unauthorized()
		Some(u) -> json(200, profile_json(u))
	}
}

fn handle_logout() Response {
	http.with_header(json(200, '{"ok":true}'), NAME_SET_COOKIE, <<'session=; Path=/; Max-Age=0'>>)
}

const USERS_PREFIX = <<'/users/'>>
const USERS_PREFIX_LEN = 7

fn handle_user_by_id(path Binary, t Tables) Response {
	n = binary.byte_size(path)
	rest = binary.slice_bytes(path, USERS_PREFIX_LEN, n - USERS_PREFIX_LEN)
	user = match binary.parse_int(rest, Dec) {
		Ok(id) -> find_by_id(t, id)
		Err(Nil) -> None
	}
	match user {
		None -> json(404, '{"error":"no such user"}')
		Some(u) -> json(200, profile_json(u))
	}
}

fn route(req Request, t Tables) Response {
	path = http.path(req)
	match req.method {
		Get -> match path {
			<<'/'>> -> respond(200, VAL_TEXT, 'hello')
			<<'/me'>> -> handle_me(req, t)
			else -> if binary.byte_size(path) > USERS_PREFIX_LEN &&
			binary.slice_bytes(path, 0, USERS_PREFIX_LEN) == USERS_PREFIX {
				handle_user_by_id(path, t)
			} else {
				json(404, '{"error":"not found"}')
			}
		}
		Post -> match path {
			<<'/login'>> -> handle_login(req, t)
			<<'/logout'>> -> handle_logout()
			else -> json(404, '{"error":"not found"}')
		}
		else -> json(405, '{"error":"method not allowed"}')
	}
}

fn port_arg() Int {
	match process.argv() {
		[_script, p, .._rest] -> binary.parse_int(binary.from_string(p), Dec) or DEFAULT_PORT
		else -> DEFAULT_PORT
	}
}

tables = build_tables(0, USER_COUNT, Tables(by_name: map.new(), by_id: map.new()))
port = port_arg()

println('bench_service listening on http://127.0.0.1:${port} (${USER_COUNT} users)')

match http.serve('127.0.0.1', port, fn(req) route(req, tables)) {
	Ok(_) -> Nil
	Err(e) -> println('serve failed: ${e}')
}
