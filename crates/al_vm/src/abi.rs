//! The value ABI: outcomes the VM constructs on its own, plus the errno
//! classification that picks between them.
//!
//! A slot names an outcome, not a constructor. The front end binds one template
//! per slot ([`Program::abi`](crate::bytecode::Program)) and the VM instantiates
//! `abi[slot]`.
//!
//! Payload order is normative: the VM reads and writes payload fields by
//! position, so the order noted on each slot is binding. Field names are free.

use crate::bytecode::Op;
use crate::newtype_index;

newtype_index!(
    /// Index into [`Program::templates`](crate::bytecode::Program::templates).
    pub struct TemplateIdx("tpl#")
);

/// One outcome the VM may construct. Discriminants must stay dense from zero:
/// the slot table is `[Option<TemplateIdx>; COUNT]` indexed by discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum AbiSlot {
    /// `[value]`
    ResultOk,
    /// `[error]`
    ResultErr,
    /// `[value]`
    OptionSome,
    OptionNone,
    Unit,

    /// `ENOENT` — `[path Str]`
    FsEnoent,
    /// `EACCES` — `[path Str]`
    FsEacces,
    /// `EEXIST` — `[path Str]`
    FsEexist,
    /// `ENOTDIR` — `[path Str]`
    FsEnotdir,
    /// `EISDIR` — `[path Str]`
    FsEisdir,
    /// `EROFS` — `[path Str]`
    FsErofs,
    /// `ELOOP` — `[path Str]`
    FsEloop,
    /// `EFBIG` — `[path Str]`
    FsEfbig,
    /// `ENOSPC`
    FsEnospc,
    /// `EDQUOT`
    FsEdquot,
    /// A write of a binary that is not a whole number of bytes. Not an errno.
    FsUnalignedBinary,
    /// Any errno with no class above — `[code Int]`
    FsErrnoOther,

    /// `ETIMEDOUT`. Also `TcpReadUntil` passing its deadline.
    NetEtimedout,
    /// `ECONNREFUSED`
    NetEconnrefused,
    /// `ECONNRESET`
    NetEconnreset,
    /// `ECONNABORTED`. Also the poller on a lost connection.
    NetEconnaborted,
    /// `ENOTCONN`
    NetEnotconn,
    /// `EPIPE`
    NetEpipe,
    /// `EADDRINUSE`
    NetEaddrinuse,
    /// `EADDRNOTAVAIL`
    NetEaddrnotavail,
    /// `ENETDOWN`
    NetEnetdown,
    /// `ENETUNREACH`
    NetEnetunreach,
    /// `EHOSTUNREACH`
    NetEhostunreach,
    /// `EACCES`
    NetEacces,
    /// A port outside 1..=65535. VM precondition.
    NetInvalidPort,
    /// VM precondition, as [`AbiSlot::FsUnalignedBinary`].
    NetUnalignedBinary,
    /// Any errno with no class above — `[code Int]`
    NetErrnoOther,

    /// `[addr Str]`
    IpV4,
    /// `[addr Str]`
    IpV6,
    /// `[ip, port Int]`
    SocketAddr,
    /// `[conn, peer]`
    Socket,
    /// `[bytes Binary]`
    ReadData,
    ReadClosed,

    /// `[name Binary, value Binary]`
    H1Header,
    H1Http10,
    H1Http11,
    /// `[conn_close Bool, conn_keep_alive Bool, expect_100_continue Bool]`
    H1HeadFlags,
    /// `[method Binary, target Binary, version, headers, flags, consumed Int]`
    H1ParsedDone,
    H1ParsedNeedMore,
    /// `[status Int]`
    H1ParsedBad,
    H1FramingNoBody,
    /// `[n Int]`
    H1FramingLength,
    H1FramingChunked,
    /// `[status Int]`
    H1FramingInvalid,
    /// `[body Binary, trailers, consumed Int]`
    H1ChunkedDone,
    H1ChunkedNeedMore,
    /// `[status Int]`
    H1ChunkedBad,
}

impl AbiSlot {
    pub const COUNT: usize = AbiSlot::H1ChunkedBad as usize + 1;

    pub const ALL: [AbiSlot; AbiSlot::COUNT] = {
        use AbiSlot::*;
        [
            ResultOk,
            ResultErr,
            OptionSome,
            OptionNone,
            Unit,
            FsEnoent,
            FsEacces,
            FsEexist,
            FsEnotdir,
            FsEisdir,
            FsErofs,
            FsEloop,
            FsEfbig,
            FsEnospc,
            FsEdquot,
            FsUnalignedBinary,
            FsErrnoOther,
            NetEtimedout,
            NetEconnrefused,
            NetEconnreset,
            NetEconnaborted,
            NetEnotconn,
            NetEpipe,
            NetEaddrinuse,
            NetEaddrnotavail,
            NetEnetdown,
            NetEnetunreach,
            NetEhostunreach,
            NetEacces,
            NetInvalidPort,
            NetUnalignedBinary,
            NetErrnoOther,
            IpV4,
            IpV6,
            SocketAddr,
            Socket,
            ReadData,
            ReadClosed,
            H1Header,
            H1Http10,
            H1Http11,
            H1HeadFlags,
            H1ParsedDone,
            H1ParsedNeedMore,
            H1ParsedBad,
            H1FramingNoBody,
            H1FramingLength,
            H1FramingChunked,
            H1FramingInvalid,
            H1ChunkedDone,
            H1ChunkedNeedMore,
            H1ChunkedBad,
        ]
    };

    pub const fn name(self) -> &'static str {
        use AbiSlot::*;
        match self {
            ResultOk => "ResultOk",
            ResultErr => "ResultErr",
            OptionSome => "OptionSome",
            OptionNone => "OptionNone",
            Unit => "Unit",
            FsEnoent => "FsEnoent",
            FsEacces => "FsEacces",
            FsEexist => "FsEexist",
            FsEnotdir => "FsEnotdir",
            FsEisdir => "FsEisdir",
            FsErofs => "FsErofs",
            FsEloop => "FsEloop",
            FsEfbig => "FsEfbig",
            FsEnospc => "FsEnospc",
            FsEdquot => "FsEdquot",
            FsUnalignedBinary => "FsUnalignedBinary",
            FsErrnoOther => "FsErrnoOther",
            NetEtimedout => "NetEtimedout",
            NetEconnrefused => "NetEconnrefused",
            NetEconnreset => "NetEconnreset",
            NetEconnaborted => "NetEconnaborted",
            NetEnotconn => "NetEnotconn",
            NetEpipe => "NetEpipe",
            NetEaddrinuse => "NetEaddrinuse",
            NetEaddrnotavail => "NetEaddrnotavail",
            NetEnetdown => "NetEnetdown",
            NetEnetunreach => "NetEnetunreach",
            NetEhostunreach => "NetEhostunreach",
            NetEacces => "NetEacces",
            NetInvalidPort => "NetInvalidPort",
            NetUnalignedBinary => "NetUnalignedBinary",
            NetErrnoOther => "NetErrnoOther",
            IpV4 => "IpV4",
            IpV6 => "IpV6",
            SocketAddr => "SocketAddr",
            Socket => "Socket",
            ReadData => "ReadData",
            ReadClosed => "ReadClosed",
            H1Header => "H1Header",
            H1Http10 => "H1Http10",
            H1Http11 => "H1Http11",
            H1HeadFlags => "H1HeadFlags",
            H1ParsedDone => "H1ParsedDone",
            H1ParsedNeedMore => "H1ParsedNeedMore",
            H1ParsedBad => "H1ParsedBad",
            H1FramingNoBody => "H1FramingNoBody",
            H1FramingLength => "H1FramingLength",
            H1FramingChunked => "H1FramingChunked",
            H1FramingInvalid => "H1FramingInvalid",
            H1ChunkedDone => "H1ChunkedDone",
            H1ChunkedNeedMore => "H1ChunkedNeedMore",
            H1ChunkedBad => "H1ChunkedBad",
        }
    }

    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The payload arity the slot's documented order fixes. A binding whose
    /// constructor disagrees is rejected at load ([`crate::template::AbiTable`]).
    pub const fn arity(self) -> usize {
        use AbiSlot::*;
        match self {
            OptionNone | Unit | FsEnospc | FsEdquot | FsUnalignedBinary | NetEtimedout
            | NetEconnrefused | NetEconnreset | NetEconnaborted | NetEnotconn | NetEpipe
            | NetEaddrinuse | NetEaddrnotavail | NetEnetdown | NetEnetunreach | NetEhostunreach
            | NetEacces | NetInvalidPort | NetUnalignedBinary | ReadClosed | H1Http10
            | H1Http11 | H1ParsedNeedMore | H1FramingNoBody | H1FramingChunked
            | H1ChunkedNeedMore => 0,
            ResultOk | ResultErr | OptionSome | FsEnoent | FsEacces | FsEexist | FsEnotdir
            | FsEisdir | FsErofs | FsEloop | FsEfbig | FsErrnoOther | NetErrnoOther | IpV4
            | IpV6 | ReadData | H1ParsedBad | H1FramingLength | H1FramingInvalid | H1ChunkedBad => {
                1
            }
            SocketAddr | Socket | H1Header => 2,
            H1HeadFlags | H1ChunkedDone => 3,
            H1ParsedDone => 6,
        }
    }
}

/// Every slot `op` may construct. A front end need only bind the slots for the
/// ops it actually emits.
pub fn slots_for(op: Op) -> &'static [AbiSlot] {
    use AbiSlot as S;
    match op {
        Op::FileRead | Op::FileWrite => &[
            S::ResultOk,
            S::ResultErr,
            S::Unit,
            S::FsEnoent,
            S::FsEacces,
            S::FsEexist,
            S::FsEnotdir,
            S::FsEisdir,
            S::FsErofs,
            S::FsEloop,
            S::FsEfbig,
            S::FsEnospc,
            S::FsEdquot,
            S::FsUnalignedBinary,
            S::FsErrnoOther,
        ],
        Op::TcpListen | Op::TcpConnect | Op::TcpCloseServer | Op::TcpClose => &[
            S::ResultOk,
            S::ResultErr,
            S::Unit,
            S::Socket,
            S::SocketAddr,
            S::IpV4,
            S::IpV6,
            S::NetEtimedout,
            S::NetEconnrefused,
            S::NetEconnreset,
            S::NetEconnaborted,
            S::NetEnotconn,
            S::NetEpipe,
            S::NetEaddrinuse,
            S::NetEaddrnotavail,
            S::NetEnetdown,
            S::NetEnetunreach,
            S::NetEhostunreach,
            S::NetEacces,
            S::NetInvalidPort,
            S::NetUnalignedBinary,
            S::NetErrnoOther,
        ],
        Op::TcpAccept => &[
            S::ResultOk,
            S::ResultErr,
            S::Socket,
            S::SocketAddr,
            S::IpV4,
            S::IpV6,
            S::NetEconnaborted,
            S::NetErrnoOther,
        ],
        Op::TcpRead | Op::TcpReadUntil => &[
            S::ResultOk,
            S::ResultErr,
            S::ReadData,
            S::ReadClosed,
            S::NetEtimedout,
            S::NetEconnreset,
            S::NetEconnaborted,
            S::NetEnotconn,
            S::NetEpipe,
            S::NetErrnoOther,
        ],
        Op::TcpWrite | Op::TcpWriteParts => &[
            S::ResultOk,
            S::ResultErr,
            S::Unit,
            S::NetEconnreset,
            S::NetEpipe,
            S::NetEnotconn,
            S::NetUnalignedBinary,
            S::NetErrnoOther,
        ],
        Op::TcpLocalAddr => &[
            S::ResultOk,
            S::ResultErr,
            S::SocketAddr,
            S::IpV4,
            S::IpV6,
            S::NetErrnoOther,
        ],
        Op::DnsResolve => &[
            S::ResultOk,
            S::ResultErr,
            S::IpV4,
            S::IpV6,
            S::NetEtimedout,
            S::NetEhostunreach,
            S::NetErrnoOther,
        ],
        Op::IpParse => &[S::OptionSome, S::OptionNone, S::IpV4, S::IpV6],
        Op::HttpParseHead => &[
            S::H1ParsedDone,
            S::H1ParsedNeedMore,
            S::H1ParsedBad,
            S::H1Header,
            S::H1Http10,
            S::H1Http11,
            S::H1HeadFlags,
        ],
        Op::HttpFraming => &[
            S::H1FramingNoBody,
            S::H1FramingLength,
            S::H1FramingChunked,
            S::H1FramingInvalid,
        ],
        Op::HttpChunkDecode => &[
            S::H1ChunkedDone,
            S::H1ChunkedNeedMore,
            S::H1ChunkedBad,
            S::H1Header,
        ],
        Op::HttpHeaderGet | Op::MapGet => &[S::OptionSome, S::OptionNone],
        Op::Sleep => &[S::Unit],
        _ => &[],
    }
}

/// A filesystem failure. The variant says what payload the caller owes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsFailure {
    Path(AbiSlot),
    Bare(AbiSlot),
    Errno(i32),
}

/// A socket failure, as [`FsFailure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetFailure {
    Bare(AbiSlot),
    Errno(i32),
}

pub fn classify_fs(e: &std::io::Error) -> FsFailure {
    match e.raw_os_error() {
        Some(libc::ENOENT) => FsFailure::Path(AbiSlot::FsEnoent),
        Some(libc::EACCES) => FsFailure::Path(AbiSlot::FsEacces),
        Some(libc::EEXIST) => FsFailure::Path(AbiSlot::FsEexist),
        Some(libc::ENOTDIR) => FsFailure::Path(AbiSlot::FsEnotdir),
        Some(libc::EISDIR) => FsFailure::Path(AbiSlot::FsEisdir),
        Some(libc::EROFS) => FsFailure::Path(AbiSlot::FsErofs),
        Some(libc::ELOOP) => FsFailure::Path(AbiSlot::FsEloop),
        Some(libc::EFBIG) => FsFailure::Path(AbiSlot::FsEfbig),
        Some(libc::ENOSPC) => FsFailure::Bare(AbiSlot::FsEnospc),
        Some(libc::EDQUOT) => FsFailure::Bare(AbiSlot::FsEdquot),
        _ => FsFailure::Errno(errno_of(e)),
    }
}

pub fn classify_net(e: &std::io::Error) -> NetFailure {
    match e.raw_os_error() {
        Some(libc::ETIMEDOUT) => NetFailure::Bare(AbiSlot::NetEtimedout),
        Some(libc::ECONNREFUSED) => NetFailure::Bare(AbiSlot::NetEconnrefused),
        Some(libc::ECONNRESET) => NetFailure::Bare(AbiSlot::NetEconnreset),
        Some(libc::ECONNABORTED) => NetFailure::Bare(AbiSlot::NetEconnaborted),
        Some(libc::ENOTCONN) => NetFailure::Bare(AbiSlot::NetEnotconn),
        Some(libc::EPIPE) => NetFailure::Bare(AbiSlot::NetEpipe),
        Some(libc::EADDRINUSE) => NetFailure::Bare(AbiSlot::NetEaddrinuse),
        Some(libc::EADDRNOTAVAIL) => NetFailure::Bare(AbiSlot::NetEaddrnotavail),
        Some(libc::ENETDOWN) => NetFailure::Bare(AbiSlot::NetEnetdown),
        Some(libc::ENETUNREACH) => NetFailure::Bare(AbiSlot::NetEnetunreach),
        Some(libc::EHOSTUNREACH) => NetFailure::Bare(AbiSlot::NetEhostunreach),
        Some(libc::EACCES) => NetFailure::Bare(AbiSlot::NetEacces),
        _ => NetFailure::Errno(errno_of(e)),
    }
}

/// The raw errno, or `-1` for a synthesised error with no OS origin.
fn errno_of(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_dense_and_ordered() {
        for (i, slot) in AbiSlot::ALL.iter().enumerate() {
            assert_eq!(slot.index(), i, "{} out of order", slot.name());
        }
        assert_eq!(AbiSlot::ALL.len(), AbiSlot::COUNT);
    }

    #[test]
    fn slot_names_are_unique() {
        let mut names: Vec<&str> = AbiSlot::ALL.iter().map(|s| s.name()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before);
    }

    #[test]
    fn fs_classification_matches_posix() {
        let e = std::io::Error::from_raw_os_error(libc::ENOENT);
        assert_eq!(classify_fs(&e), FsFailure::Path(AbiSlot::FsEnoent));
        let e = std::io::Error::from_raw_os_error(libc::ENOSPC);
        assert_eq!(classify_fs(&e), FsFailure::Bare(AbiSlot::FsEnospc));
        let e = std::io::Error::from_raw_os_error(libc::EIO);
        assert_eq!(classify_fs(&e), FsFailure::Errno(libc::EIO));
    }

    #[test]
    fn net_classification_matches_posix() {
        let e = std::io::Error::from_raw_os_error(libc::ECONNREFUSED);
        assert_eq!(classify_net(&e), NetFailure::Bare(AbiSlot::NetEconnrefused));
        let e = std::io::Error::from_raw_os_error(libc::EIO);
        assert_eq!(classify_net(&e), NetFailure::Errno(libc::EIO));
    }

    #[test]
    fn errorless_io_error_gets_the_residual() {
        let e = std::io::Error::other("no os origin");
        assert_eq!(classify_fs(&e), FsFailure::Errno(-1));
        assert_eq!(classify_net(&e), NetFailure::Errno(-1));
    }

    #[test]
    fn fallible_ops_declare_their_result_wrappers() {
        for op in [Op::FileRead, Op::TcpConnect, Op::DnsResolve] {
            let slots = slots_for(op);
            assert!(slots.contains(&AbiSlot::ResultOk), "{op:?}");
            assert!(slots.contains(&AbiSlot::ResultErr), "{op:?}");
        }
        assert!(slots_for(Op::Add).is_empty());
    }
}
