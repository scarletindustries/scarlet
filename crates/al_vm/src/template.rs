//! Descriptors for the enum values the VM constructs on its own — `Ok`/`Err`
//! results from I/O ops, `NetError` variants, HTTP protocol values, and the
//! like. A [`VariantTemplate`] is pure data (a nominal id plus names and
//! labels); the front end supplies one per variant it wants the runtime to be
//! able to build, so the VM never hard-codes a language's stdlib.

use crate::TypeId;

/// Static descriptor for one constructor the runtime may instantiate (type
/// id, type/variant name, field labels). AL's build.rs emits these as
/// `stdlib::<module>::<CTOR>` consts so a rename in the AL source surfaces as
/// a Rust compile error at the VM usage site rather than silently
/// constructing a mismatched value.
#[derive(Debug)]
pub struct VariantTemplate {
    pub type_id: TypeId,
    pub variant_idx: u16,
    pub type_name: &'static str,
    pub variant_name: &'static str,
    pub labels: &'static [&'static str],
}

/// Every constructor the VM builds of its own accord, as one table the
/// embedder assembles and hands to [`vm::new_vm`](crate::vm::new_vm). This is
/// the whole of the VM's knowledge of a stdlib: swap the table and the same
/// opcodes construct another language's values. Field groups mirror the AL
/// stdlib modules the templates come from.
///
/// Fields are `&'static` because the table outlives every scheduler thread
/// and because template identity is the *address*: `VM::stdlib_template`
/// memoizes per-template frozen name interning keyed on the reference, so
/// each template must have exactly one home.
#[derive(Debug)]
pub struct StdlibTemplates {
    pub prelude: PreludeVariants,
    pub io_error: IoErrorVariants,
    pub net_address: NetAddressVariants,
    pub net_socket: NetSocketVariants,
    pub net_error: NetErrorVariants,
    pub http: HttpVariants,
}

/// Core prelude constructors: `Nil`, `Option`, `Result`.
#[derive(Debug)]
pub struct PreludeVariants {
    pub nil: &'static VariantTemplate,
    pub none: &'static VariantTemplate,
    pub some: &'static VariantTemplate,
    pub ok: &'static VariantTemplate,
    pub err: &'static VariantTemplate,
}

/// `al/io.Error` variants for `FileRead`/`FileWrite` failures.
#[derive(Debug)]
pub struct IoErrorVariants {
    pub not_found: &'static VariantTemplate,
    pub permission_denied: &'static VariantTemplate,
    pub already_exists: &'static VariantTemplate,
    pub not_a_directory: &'static VariantTemplate,
    pub is_a_directory: &'static VariantTemplate,
    pub read_only_filesystem: &'static VariantTemplate,
    pub filesystem_loop: &'static VariantTemplate,
    pub storage_full: &'static VariantTemplate,
    pub quota_exceeded: &'static VariantTemplate,
    pub file_too_large: &'static VariantTemplate,
    pub unaligned_binary: &'static VariantTemplate,
    pub errno: &'static VariantTemplate,
}

/// `al/net/address` constructors.
#[derive(Debug)]
pub struct NetAddressVariants {
    pub v4: &'static VariantTemplate,
    pub v6: &'static VariantTemplate,
    pub socket_address: &'static VariantTemplate,
}

/// `al/net/socket` constructors.
#[derive(Debug)]
pub struct NetSocketVariants {
    pub socket: &'static VariantTemplate,
    pub data: &'static VariantTemplate,
    pub closed: &'static VariantTemplate,
}

/// `al/net.NetError` variants for socket/DNS failures.
#[derive(Debug)]
pub struct NetErrorVariants {
    pub timed_out: &'static VariantTemplate,
    pub connection_refused: &'static VariantTemplate,
    pub connection_reset: &'static VariantTemplate,
    pub connection_aborted: &'static VariantTemplate,
    pub not_connected: &'static VariantTemplate,
    pub broken_pipe: &'static VariantTemplate,
    pub addr_in_use: &'static VariantTemplate,
    pub addr_not_available: &'static VariantTemplate,
    pub network_down: &'static VariantTemplate,
    pub network_unreachable: &'static VariantTemplate,
    pub host_unreachable: &'static VariantTemplate,
    pub permission_denied: &'static VariantTemplate,
    pub invalid_port: &'static VariantTemplate,
    pub unaligned_binary: &'static VariantTemplate,
    pub errno: &'static VariantTemplate,
}

/// `al/http/headers` + `al/http/h1` protocol constructors for the native
/// HTTP scanners.
#[derive(Debug)]
pub struct HttpVariants {
    pub header: &'static VariantTemplate,
    pub http10: &'static VariantTemplate,
    pub http11: &'static VariantTemplate,
    pub head_flags: &'static VariantTemplate,
    pub parsed_done: &'static VariantTemplate,
    pub parsed_need_more: &'static VariantTemplate,
    pub parsed_bad: &'static VariantTemplate,
    pub framing_no_body: &'static VariantTemplate,
    pub framing_length: &'static VariantTemplate,
    pub framing_chunked: &'static VariantTemplate,
    pub framing_invalid: &'static VariantTemplate,
    pub chunked_done: &'static VariantTemplate,
    pub chunked_need_more: &'static VariantTemplate,
    pub chunked_bad: &'static VariantTemplate,
}

/// A self-contained table for the runtime's own tests: the AL stdlib's names
/// and ids, written out by hand with no compiler in sight. Incidentally the
/// proof that the VM runs against any table the embedder supplies.
#[cfg(test)]
pub(crate) mod test_fixture {
    use super::*;

    macro_rules! vt {
        ($name:ident, $tid:expr, $vidx:expr, $ty:literal, $variant:literal, [$($label:literal),*]) => {
            static $name: VariantTemplate = VariantTemplate {
                type_id: TypeId($tid),
                variant_idx: $vidx,
                type_name: $ty,
                variant_name: $variant,
                labels: &[$($label),*],
            };
        };
    }

    vt!(NIL, 262, 0, "Nil", "Nil", []);
    vt!(SOME, 263, 0, "Option", "Some", ["value"]);
    vt!(NONE, 263, 1, "Option", "None", []);
    vt!(OK, 264, 0, "Result", "Ok", ["value"]);
    vt!(ERR, 264, 1, "Result", "Err", ["error"]);

    vt!(IO_NOT_FOUND, 5632, 0, "IoError", "NotFound", ["path"]);
    vt!(
        IO_PERMISSION_DENIED,
        5632,
        1,
        "IoError",
        "PermissionDenied",
        ["path"]
    );
    vt!(
        IO_ALREADY_EXISTS,
        5632,
        2,
        "IoError",
        "AlreadyExists",
        ["path"]
    );
    vt!(
        IO_NOT_A_DIRECTORY,
        5632,
        3,
        "IoError",
        "NotADirectory",
        ["path"]
    );
    vt!(
        IO_IS_A_DIRECTORY,
        5632,
        4,
        "IoError",
        "IsADirectory",
        ["path"]
    );
    vt!(
        IO_READ_ONLY_FILESYSTEM,
        5632,
        5,
        "IoError",
        "ReadOnlyFilesystem",
        ["path"]
    );
    vt!(
        IO_FILESYSTEM_LOOP,
        5632,
        6,
        "IoError",
        "FilesystemLoop",
        ["path"]
    );
    vt!(IO_STORAGE_FULL, 5632, 8, "IoError", "StorageFull", []);
    vt!(IO_QUOTA_EXCEEDED, 5632, 9, "IoError", "QuotaExceeded", []);
    vt!(
        IO_FILE_TOO_LARGE,
        5632,
        10,
        "IoError",
        "FileTooLarge",
        ["path"]
    );
    vt!(
        IO_UNALIGNED_BINARY,
        5632,
        11,
        "IoError",
        "UnalignedBinary",
        []
    );
    vt!(IO_ERRNO, 5632, 12, "IoError", "Errno", ["code"]);

    vt!(ADDR_V4, 3328, 0, "IpAddress", "V4", ["addr"]);
    vt!(ADDR_V6, 3328, 1, "IpAddress", "V6", ["addr"]);
    vt!(
        SOCKET_ADDRESS,
        3329,
        0,
        "SocketAddress",
        "SocketAddress",
        ["ip", "port"]
    );

    vt!(SOCKET, 4097, 0, "Socket", "Socket", ["conn", "peer"]);
    vt!(READ_DATA, 4098, 0, "Read", "Data", ["bytes"]);
    vt!(READ_CLOSED, 4098, 1, "Read", "Closed", []);

    vt!(NET_TIMED_OUT, 3584, 0, "NetError", "TimedOut", []);
    vt!(
        NET_CONNECTION_REFUSED,
        3584,
        1,
        "NetError",
        "ConnectionRefused",
        []
    );
    vt!(
        NET_CONNECTION_RESET,
        3584,
        2,
        "NetError",
        "ConnectionReset",
        []
    );
    vt!(
        NET_CONNECTION_ABORTED,
        3584,
        3,
        "NetError",
        "ConnectionAborted",
        []
    );
    vt!(NET_NOT_CONNECTED, 3584, 4, "NetError", "NotConnected", []);
    vt!(NET_BROKEN_PIPE, 3584, 5, "NetError", "BrokenPipe", []);
    vt!(NET_ADDR_IN_USE, 3584, 6, "NetError", "AddrInUse", []);
    vt!(
        NET_ADDR_NOT_AVAILABLE,
        3584,
        7,
        "NetError",
        "AddrNotAvailable",
        []
    );
    vt!(NET_NETWORK_DOWN, 3584, 8, "NetError", "NetworkDown", []);
    vt!(
        NET_NETWORK_UNREACHABLE,
        3584,
        9,
        "NetError",
        "NetworkUnreachable",
        []
    );
    vt!(
        NET_HOST_UNREACHABLE,
        3584,
        10,
        "NetError",
        "HostUnreachable",
        []
    );
    vt!(
        NET_PERMISSION_DENIED,
        3584,
        11,
        "NetError",
        "PermissionDenied",
        []
    );
    vt!(
        NET_UNALIGNED_BINARY,
        3584,
        14,
        "NetError",
        "UnalignedBinary",
        []
    );
    vt!(NET_INVALID_PORT, 3584, 15, "NetError", "InvalidPort", []);
    vt!(NET_ERRNO, 3584, 16, "NetError", "Errno", ["code"]);

    vt!(HEADER, 2560, 0, "Header", "Header", ["name", "value"]);
    vt!(HTTP10, 3072, 0, "Version", "Http10", []);
    vt!(HTTP11, 3072, 1, "Version", "Http11", []);
    vt!(
        HEAD_FLAGS,
        3073,
        0,
        "HeadFlags",
        "HeadFlags",
        ["conn_close", "conn_keep_alive", "expect_100_continue"]
    );
    vt!(
        PARSED_DONE,
        3074,
        0,
        "Parsed",
        "Done",
        [
            "method", "target", "version", "headers", "flags", "consumed"
        ]
    );
    vt!(PARSED_NEED_MORE, 3074, 1, "Parsed", "NeedMore", []);
    vt!(PARSED_BAD, 3074, 2, "Parsed", "Bad", ["status"]);
    vt!(FRAMING_NO_BODY, 3075, 0, "Framing", "NoBody", []);
    vt!(FRAMING_LENGTH, 3075, 1, "Framing", "Length", ["n"]);
    vt!(FRAMING_CHUNKED, 3075, 2, "Framing", "Chunked", []);
    vt!(FRAMING_INVALID, 3075, 3, "Framing", "Invalid", ["status"]);
    vt!(
        CHUNKED_DONE,
        3076,
        0,
        "ChunkBody",
        "ChunkedDone",
        ["body", "trailers", "consumed"]
    );
    vt!(
        CHUNKED_NEED_MORE,
        3076,
        1,
        "ChunkBody",
        "ChunkedNeedMore",
        []
    );
    vt!(CHUNKED_BAD, 3076, 2, "ChunkBody", "ChunkedBad", ["status"]);

    pub(crate) static TEST_STDLIB: StdlibTemplates = StdlibTemplates {
        prelude: PreludeVariants {
            nil: &NIL,
            none: &NONE,
            some: &SOME,
            ok: &OK,
            err: &ERR,
        },
        io_error: IoErrorVariants {
            not_found: &IO_NOT_FOUND,
            permission_denied: &IO_PERMISSION_DENIED,
            already_exists: &IO_ALREADY_EXISTS,
            not_a_directory: &IO_NOT_A_DIRECTORY,
            is_a_directory: &IO_IS_A_DIRECTORY,
            read_only_filesystem: &IO_READ_ONLY_FILESYSTEM,
            filesystem_loop: &IO_FILESYSTEM_LOOP,
            storage_full: &IO_STORAGE_FULL,
            quota_exceeded: &IO_QUOTA_EXCEEDED,
            file_too_large: &IO_FILE_TOO_LARGE,
            unaligned_binary: &IO_UNALIGNED_BINARY,
            errno: &IO_ERRNO,
        },
        net_address: NetAddressVariants {
            v4: &ADDR_V4,
            v6: &ADDR_V6,
            socket_address: &SOCKET_ADDRESS,
        },
        net_socket: NetSocketVariants {
            socket: &SOCKET,
            data: &READ_DATA,
            closed: &READ_CLOSED,
        },
        net_error: NetErrorVariants {
            timed_out: &NET_TIMED_OUT,
            connection_refused: &NET_CONNECTION_REFUSED,
            connection_reset: &NET_CONNECTION_RESET,
            connection_aborted: &NET_CONNECTION_ABORTED,
            not_connected: &NET_NOT_CONNECTED,
            broken_pipe: &NET_BROKEN_PIPE,
            addr_in_use: &NET_ADDR_IN_USE,
            addr_not_available: &NET_ADDR_NOT_AVAILABLE,
            network_down: &NET_NETWORK_DOWN,
            network_unreachable: &NET_NETWORK_UNREACHABLE,
            host_unreachable: &NET_HOST_UNREACHABLE,
            permission_denied: &NET_PERMISSION_DENIED,
            invalid_port: &NET_INVALID_PORT,
            unaligned_binary: &NET_UNALIGNED_BINARY,
            errno: &NET_ERRNO,
        },
        http: HttpVariants {
            header: &HEADER,
            http10: &HTTP10,
            http11: &HTTP11,
            head_flags: &HEAD_FLAGS,
            parsed_done: &PARSED_DONE,
            parsed_need_more: &PARSED_NEED_MORE,
            parsed_bad: &PARSED_BAD,
            framing_no_body: &FRAMING_NO_BODY,
            framing_length: &FRAMING_LENGTH,
            framing_chunked: &FRAMING_CHUNKED,
            framing_invalid: &FRAMING_INVALID,
            chunked_done: &CHUNKED_DONE,
            chunked_need_more: &CHUNKED_NEED_MORE,
            chunked_bad: &CHUNKED_BAD,
        },
    };
}
