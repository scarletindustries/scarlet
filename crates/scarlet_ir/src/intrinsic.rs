//! The built-in functions the language provides. A stdlib declaration marked
//! `@vm(key)` has no Scarlet body, and its `key` names one of these.
//!
//! Analysis turns the key into an [`Intrinsic`] once, at the annotation, so an
//! unknown key is a compile error there and everything after carries the
//! variant rather than the string. What an intrinsic does is the runtime's
//! business; this list only names them.

macro_rules! intrinsics {
    ($($variant:ident = $key:literal,)*) => {
        /// One built-in function, named by its `@vm(key)`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Intrinsic {
            $($variant,)*
        }

        impl Intrinsic {
            /// The intrinsic an `@vm(key)` names, or `None` for an unknown key.
            pub fn from_key(key: &str) -> Option<Intrinsic> {
                Some(match key {
                    $($key => Intrinsic::$variant,)*
                    _ => return None,
                })
            }
        }

        #[cfg(test)]
        const PAIRS: &[(Intrinsic, &str)] = &[$((Intrinsic::$variant, $key),)*];
    };
}

intrinsics! {
    Println = "println",
    StringInspect = "string__inspect",
    InternalStackDepth = "internal__stack_depth",
    InternalLiveSubjects = "internal__live_subjects",
    InternalBlockingThreads = "internal__blocking_threads",
    IoReadFile = "io__read_file",
    IoWriteFile = "io__write_file",
    NetListen = "net__listen",
    NetAccept = "net__accept",
    NetConnect = "net__connect",
    NetConnectUntil = "net__connect_until",
    NetClose = "net__close",
    NetGive = "net__give",
    NetLocalAddr = "net__local_addr",
    NetResolve = "net__resolve",
    NetResolveUntil = "net__resolve_until",
    AddressParse = "address__parse",
    SocketRead = "socket__read",
    SocketReadUntil = "socket__read_until",
    SocketWrite = "socket__write",
    SocketWriteParts = "socket__write_parts",
    SocketClose = "socket__close",
    TlsHandshake = "tls__handshake",
    TlsHandshakeUntil = "tls__handshake_until",
    TlsRead = "tls__read",
    TlsReadUntil = "tls__read_until",
    TlsWrite = "tls__write",
    TlsClose = "tls__close",
    PortSpawn = "port__spawn",
    PortRead = "port__read",
    PortReadUntil = "port__read_until",
    PortWrite = "port__write",
    PortWriteParts = "port__write_parts",
    PortClose = "port__close",
    StringSplit = "string__split",
    StringLength = "string__length",
    StringContains = "string__contains",
    StringTrim = "string__trim",
    StringToGraphemes = "string__to_graphemes",
    IntToString = "int__to_string",
    IntFromString = "int__from_string",
    IntBitwiseAnd = "int__bitwise_and",
    IntBitwiseOr = "int__bitwise_or",
    IntBitwiseXor = "int__bitwise_xor",
    IntBitwiseNot = "int__bitwise_not",
    IntBitwiseShiftLeft = "int__bitwise_shift_left",
    IntBitwiseShiftRight = "int__bitwise_shift_right",
    ArrayLength = "array__length",
    BinaryFromString = "binary__from_string",
    BinaryToString = "binary__to_string",
    BinaryBitSize = "binary__bit_size",
    BinaryByteSize = "binary__byte_size",
    BinarySliceBits = "binary__slice_bits",
    BinaryAppend = "binary__append",
    BinaryIndexOf = "binary__index_of",
    BinaryByteAt = "binary__byte_at",
    BinaryParseInt = "binary__parse_int",
    BinaryEqIgnoreAsciiCase = "binary__eq_ignore_ascii_case",
    BinaryToAsciiLower = "binary__to_ascii_lower",
    BinaryFromIntAscii = "binary__from_int_ascii",
    HttpParseHead = "http__parse_head",
    HttpParseResponseHead = "http__parse_response_head",
    HttpFraming = "http__framing",
    HttpChunkDecode = "http__chunk_decode",
    HttpHeaderGet = "http__header_get",
    HttpHeaderHas = "http__header_has",
    HttpHeadersValid = "http__headers_valid",
    HttpSerializeHead = "http__serialize_head",
    FloatFloor = "float__floor",
    FloatCeil = "float__ceil",
    FloatRound = "float__round",
    FloatTruncate = "float__truncate",
    FloatFromInt = "float__from_int",
    FloatToString = "float__to_string",
    ProcessSpawn = "process__spawn",
    ProcessSpawnUnlinked = "process__spawn_unlinked",
    ProcessKill = "process__kill",
    ProcessSelf = "process__self",
    ProcessMonitor = "process__monitor",
    ProcessDemonitor = "process__demonitor",
    ProcessSupervisor = "process__supervisor",
    ProcessWorker = "process__worker",
    ProcessFactory = "process__factory",
    ProcessLookupOrStart = "process__lookup_or_start",
    ProcessLookup = "process__lookup",
    ProcessSupervised = "process__supervised",
    ProcessParent = "process__parent",
    ProcessChildren = "process__children",
    ProcessCount = "process__count",
    ProcessInfo = "process__info",
    ProcessWatch = "process__watch",
    ProcessUnwatch = "process__unwatch",
    ProcessWorkerOnEach = "process__worker_on_each",
    ProcessStartIn = "process__start_in",
    ProcessSleep = "process__sleep",
    ProcessSubject = "process__subject",
    ProcessSend = "process__send",
    ProcessSendUrgent = "process__send_urgent",
    ProcessReceive = "process__receive",
    ProcessReceiveUntil = "process__receive_until",
    TimeMonotonic = "time__monotonic",
    TimeEpochMs = "time__epoch_ms",
    CryptoRandomBytes = "crypto__random_bytes",
    CryptoSha1 = "crypto__sha1",
    CryptoSha256 = "crypto__sha256",
    CryptoSha512 = "crypto__sha512",
    CryptoHmacSha256 = "crypto__hmac_sha256",
    CryptoConstEq = "crypto__const_eq",
    CryptoP256Verify = "crypto__p256_verify",
    CryptoEd25519Verify = "crypto__ed25519_verify",
    OsArgv = "os__argv",
    OsEnv = "os__env",
    MapGet = "map__get",
    MapHas = "map__has",
    MapKeys = "map__keys",
    MapValues = "map__values",
    MapSize = "map__size",
    MapNew = "map__new",
    MapSet = "map__set",
    MapDelete = "map__delete",
    MapToList = "map__to_list",
    JsonParseBinary = "json__parse_binary",
    JsonKind = "json__kind",
    JsonLen = "json__len",
    JsonField = "json__field",
    JsonIndex = "json__index",
    JsonEntries = "json__entries",
    JsonElements = "json__elements",
    JsonString = "json__string",
    JsonInt = "json__int",
    JsonIntText = "json__int_text",
    JsonFloat = "json__float",
    JsonBool = "json__bool",
    JsonEncode = "json__encode",
    WireEncode = "wire__encode",
    WireDecode = "wire__decode",
}

#[cfg(test)]
mod tests {
    use super::{Intrinsic, PAIRS};

    #[test]
    fn every_key_names_its_own_variant() {
        for &(intrinsic, key) in PAIRS {
            assert_eq!(Intrinsic::from_key(key), Some(intrinsic), "{key}");
        }
    }

    #[test]
    fn an_unknown_key_names_nothing() {
        assert_eq!(Intrinsic::from_key("no_such_intrinsic"), None);
    }
}
