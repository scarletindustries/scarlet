//! AL's embedding of the language-agnostic VM. The one place AL's stdlib
//! identities meet the runtime; the VM itself cannot name them.

pub use al_vm::vm::*;

use al_core::bytecode::Program;
use al_vm::template::{
    HttpVariants, IoErrorVariants, NetAddressVariants, NetErrorVariants, NetSocketVariants,
    PreludeVariants, StdlibTemplates,
};

use crate::stdlib;

/// The AL stdlib's constructor table. Each template must have exactly one
/// home here: `VM::stdlib_template` memoizes on the address.
pub static STDLIB_TEMPLATES: StdlibTemplates = StdlibTemplates {
    prelude: PreludeVariants {
        nil: &stdlib::prelude::NIL,
        none: &stdlib::prelude::NONE,
        some: &stdlib::prelude::SOME,
        ok: &stdlib::prelude::OK,
        err: &stdlib::prelude::ERR,
    },
    io_error: IoErrorVariants {
        not_found: &stdlib::io::NOT_FOUND,
        permission_denied: &stdlib::io::PERMISSION_DENIED,
        already_exists: &stdlib::io::ALREADY_EXISTS,
        not_a_directory: &stdlib::io::NOT_ADIRECTORY,
        is_a_directory: &stdlib::io::IS_ADIRECTORY,
        read_only_filesystem: &stdlib::io::READ_ONLY_FILESYSTEM,
        filesystem_loop: &stdlib::io::FILESYSTEM_LOOP,
        storage_full: &stdlib::io::STORAGE_FULL,
        quota_exceeded: &stdlib::io::QUOTA_EXCEEDED,
        file_too_large: &stdlib::io::FILE_TOO_LARGE,
        unaligned_binary: &stdlib::io::UNALIGNED_BINARY,
        errno: &stdlib::io::ERRNO,
    },
    net_address: NetAddressVariants {
        v4: &stdlib::net::address::V4,
        v6: &stdlib::net::address::V6,
        socket_address: &stdlib::net::address::SOCKET_ADDRESS,
    },
    net_socket: NetSocketVariants {
        socket: &stdlib::net::socket::SOCKET,
        data: &stdlib::net::socket::DATA,
        closed: &stdlib::net::socket::CLOSED,
    },
    net_error: NetErrorVariants {
        timed_out: &stdlib::net::error::TIMED_OUT,
        connection_refused: &stdlib::net::error::CONNECTION_REFUSED,
        connection_reset: &stdlib::net::error::CONNECTION_RESET,
        connection_aborted: &stdlib::net::error::CONNECTION_ABORTED,
        not_connected: &stdlib::net::error::NOT_CONNECTED,
        broken_pipe: &stdlib::net::error::BROKEN_PIPE,
        addr_in_use: &stdlib::net::error::ADDR_IN_USE,
        addr_not_available: &stdlib::net::error::ADDR_NOT_AVAILABLE,
        network_down: &stdlib::net::error::NETWORK_DOWN,
        network_unreachable: &stdlib::net::error::NETWORK_UNREACHABLE,
        host_unreachable: &stdlib::net::error::HOST_UNREACHABLE,
        permission_denied: &stdlib::net::error::PERMISSION_DENIED,
        invalid_port: &stdlib::net::error::INVALID_PORT,
        unaligned_binary: &stdlib::net::error::UNALIGNED_BINARY,
        errno: &stdlib::net::error::ERRNO,
    },
    http: HttpVariants {
        header: &stdlib::http::headers::HEADER,
        http10: &stdlib::http::h1::HTTP10,
        http11: &stdlib::http::h1::HTTP11,
        head_flags: &stdlib::http::h1::HEAD_FLAGS,
        parsed_done: &stdlib::http::h1::DONE,
        parsed_need_more: &stdlib::http::h1::NEED_MORE,
        parsed_bad: &stdlib::http::h1::BAD,
        framing_no_body: &stdlib::http::h1::NO_BODY,
        framing_length: &stdlib::http::h1::LENGTH,
        framing_chunked: &stdlib::http::h1::CHUNKED,
        framing_invalid: &stdlib::http::h1::INVALID,
        chunked_done: &stdlib::http::h1::CHUNKED_DONE,
        chunked_need_more: &stdlib::http::h1::CHUNKED_NEED_MORE,
        chunked_bad: &stdlib::http::h1::CHUNKED_BAD,
    },
};

/// [`al_vm::vm::new_vm`] with AL's stdlib table wired in.
pub fn new_vm(program: Program) -> VmResult<VM> {
    al_vm::vm::new_vm(program, &STDLIB_TEMPLATES)
}

/// [`al_vm::vm::new_vm_with_argv`] with AL's stdlib table wired in.
pub fn new_vm_with_argv(program: Program, argv: Vec<String>) -> VmResult<VM> {
    al_vm::vm::new_vm_with_argv(program, &STDLIB_TEMPLATES, argv)
}
