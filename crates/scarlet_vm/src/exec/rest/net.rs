//! The network built-ins: `scarlet/net`, its `address`, `socket` and `tls`.
//!
//! A listener, a connection or a TLS session lives in the host, and the
//! program holds a handle cell naming it. `Socket` and `TlsSocket` are
//! constructors holding that cell first, then the peer's address.

use scarlet_ir::core_ir::{AddressTypes, NetTypes, TlsTypes};
use scarlet_ir::intrinsic::Intrinsic;

use super::{Built, Operands, Rest};
use crate::Stop;
use crate::array;
use crate::bigint::{self, Int};
use crate::binary;
use crate::heap::Kind;
use crate::host::{Deadline, Handle, IpAddr, NetError, Received, SocketAddr, TlsError};
use crate::stop::{Broken, Missing, fault};
use crate::value::Value;

impl Rest<'_, '_> {
    /// What network built-in `i` gives for the values in `args`.
    pub(super) fn net_builtin(&mut self, i: Intrinsic, args: Operands<'_>) -> Result<Value, Stop> {
        self.let_go_of_handles();
        let v = args.get(0);
        match i {
            Intrinsic::AddressParse => match self.utf8_of(v)?.parse::<IpAddr>() {
                Ok(ip) => {
                    let ip = self.ip_value(ip)?;
                    self.some(ip)
                }
                Err(_) => Ok(self.code.abi.none),
            },
            Intrinsic::NetResolve | Intrinsic::NetResolveUntil => {
                let name = self.utf8_of(v)?;
                let deadline = match i {
                    Intrinsic::NetResolveUntil => Some(self.deadline(args.get(1))?),
                    _ => None,
                };
                match self.host.resolve(&name, deadline) {
                    Ok(ip) => {
                        let ip = self.ip_value(ip)?;
                        self.ok(ip)
                    }
                    Err(e) => self.net_err(e),
                }
            }
            Intrinsic::NetListen => {
                let Some(addr) = self.socket_addr(v)? else {
                    return self.invalid_port();
                };
                match self.host.listen(addr) {
                    Ok(h) => {
                        let server = self.handle_value(h)?;
                        self.ok(server)
                    }
                    Err(e) => self.net_err(e),
                }
            }
            Intrinsic::NetAccept => {
                let server = self.handle_of(v)?;
                match self.host.accept(server) {
                    Ok(Some((conn, peer))) => {
                        let peer = self.socket_addr_value(peer)?;
                        let sock = self.socket_value(conn, peer)?;
                        let some = self.some(sock)?;
                        self.ok(some)
                    }
                    Ok(None) => self.ok(self.code.abi.none),
                    Err(e) => self.net_err(e),
                }
            }
            Intrinsic::NetConnect | Intrinsic::NetConnectUntil => {
                let Some(addr) = self.socket_addr(v)? else {
                    return self.invalid_port();
                };
                let deadline = match i {
                    Intrinsic::NetConnectUntil => Some(self.deadline(args.get(1))?),
                    _ => None,
                };
                match self.host.connect(addr, deadline) {
                    Ok(conn) => {
                        let peer = self.share(v)?;
                        let sock = self.socket_value(conn, peer)?;
                        self.ok(sock)
                    }
                    Err(e) => self.net_err(e),
                }
            }
            Intrinsic::NetLocalAddr => {
                let server = self.handle_of(v)?;
                match self.host.local_addr(server) {
                    Ok(addr) => {
                        let addr = self.socket_addr_value(addr)?;
                        self.ok(addr)
                    }
                    Err(e) => self.net_err(e),
                }
            }
            Intrinsic::NetClose => {
                let server = self.handle_of(v)?;
                match self.host.close(server) {
                    Ok(()) => self.ok(Value::NIL),
                    Err(e) => self.net_err(e),
                }
            }
            Intrinsic::SocketRead | Intrinsic::SocketReadUntil => {
                let conn = self.conn_of(v)?;
                let max = self.read_max(args.get(1))?;
                let deadline = match i {
                    Intrinsic::SocketReadUntil => Some(self.deadline(args.get(2))?),
                    _ => None,
                };
                match self.host.read(conn, max, deadline) {
                    Ok(got) => {
                        let got = self.received(got)?;
                        self.ok(got)
                    }
                    Err(e) => self.net_err(e),
                }
            }
            Intrinsic::SocketWrite => {
                let conn = self.conn_of(v)?;
                let Some(bytes) = self.whole_bytes(args.get(1))? else {
                    let unaligned = self.net_types()?.unaligned_binary;
                    return self.err(unaligned);
                };
                match self.host.write(conn, &[&bytes]) {
                    Ok(()) => self.ok(Value::NIL),
                    Err(e) => self.net_err(e),
                }
            }
            Intrinsic::SocketWriteParts => {
                let conn = self.conn_of(v)?;
                let parts = args.get(1);
                let mut bytes = Vec::new();
                let Some(cell) = parts.as_cell() else {
                    return Err(Stop::broken(Broken::NotAnArray(parts.view())));
                };
                for part in array::elements(&self.heap, cell) {
                    let Some(b) = self.whole_bytes(part)? else {
                        let unaligned = self.net_types()?.unaligned_binary;
                        return self.err(unaligned);
                    };
                    bytes.push(b);
                }
                let slices: Vec<&[u8]> = bytes.iter().map(Vec::as_slice).collect();
                match self.host.write(conn, &slices) {
                    Ok(()) => self.ok(Value::NIL),
                    Err(e) => self.net_err(e),
                }
            }
            Intrinsic::SocketClose => {
                let conn = self.conn_of(v)?;
                match self.host.close(conn) {
                    Ok(()) => self.ok(Value::NIL),
                    Err(e) => self.net_err(e),
                }
            }
            Intrinsic::TlsHandshake | Intrinsic::TlsHandshakeUntil => {
                let conn = self.conn_of(v)?;
                let name = args.get(1);
                let server_name = self.utf8_of(name)?;
                let deadline = match i {
                    Intrinsic::TlsHandshakeUntil => Some(self.deadline(args.get(2))?),
                    _ => None,
                };
                match self.host.tls_handshake(conn, &server_name, deadline) {
                    Ok(session) => {
                        let tls = self.tls_types()?;
                        let session = self.handle_value(session)?;
                        let peer = self.field_of(v, 1)?;
                        let peer = self.share(peer)?;
                        let name = self.share(name)?;
                        let socket = self.build(tls.tls_socket, [session, peer, name])?;
                        self.ok(socket)
                    }
                    Err(e) => self.tls_err(e),
                }
            }
            Intrinsic::TlsRead | Intrinsic::TlsReadUntil => {
                let conn = self.conn_of(v)?;
                let max = self.read_max(args.get(1))?;
                let deadline = match i {
                    Intrinsic::TlsReadUntil => Some(self.deadline(args.get(2))?),
                    _ => None,
                };
                match self.host.tls_read(conn, max, deadline) {
                    Ok(got) => {
                        let got = self.received(got)?;
                        self.ok(got)
                    }
                    Err(e) => self.tls_err(e),
                }
            }
            Intrinsic::TlsWrite => {
                let conn = self.conn_of(v)?;
                let Some(bytes) = self.whole_bytes(args.get(1))? else {
                    let unaligned = self.net_types()?.unaligned_binary;
                    let transport = self.tls_types()?.transport;
                    let e = self.build(transport, [unaligned])?;
                    return self.err(e);
                };
                match self.host.tls_write(conn, &bytes) {
                    Ok(()) => self.ok(Value::NIL),
                    Err(e) => self.tls_err(e),
                }
            }
            Intrinsic::TlsClose => {
                let conn = self.conn_of(v)?;
                match self.host.tls_close(conn) {
                    Ok(()) => self.ok(Value::NIL),
                    Err(e) => self.tls_err(e),
                }
            }
            other => Err(Stop::missing(Missing::Intrinsic(other))),
        }
    }

    /// Tell the host about every handle nothing holds any more.
    pub(in crate::exec) fn let_go_of_handles(&mut self) {
        for h in self.heap.take_released() {
            self.host.release(Handle(h));
        }
    }

    fn address_types(&self) -> Result<AddressTypes<Built>, Stop> {
        self.code
            .abi
            .address
            .ok_or(Stop::broken(Broken::NoAddressTypes))
    }

    fn net_types(&self) -> Result<NetTypes<Built>, Stop> {
        self.code.abi.net.ok_or(Stop::broken(Broken::NoNetTypes))
    }

    fn tls_types(&self) -> Result<TlsTypes<Built>, Stop> {
        self.code.abi.tls.ok_or(Stop::broken(Broken::NoTlsTypes))
    }

    fn handle_value(&mut self, h: Handle) -> Result<Value, Stop> {
        Ok(Value::cell(
            self.heap.make(Kind::Handle, &[h.0]).map_err(fault)?,
        ))
    }

    /// The handle `v` is: a `Server`, a `Connection` or a `TlsConnection`.
    fn handle_of(&self, v: Value) -> Result<Handle, Stop> {
        v.as_cell()
            .filter(|c| self.heap.is(*c, Kind::Handle))
            .and_then(|c| self.heap.data(c).first().copied())
            .map(Handle)
            .ok_or_else(|| Stop::broken(Broken::NotAHandle(v.view())))
    }

    /// Field `i` of the constructor `v`.
    fn field_of(&self, v: Value, i: usize) -> Result<Value, Stop> {
        v.as_cell()
            .filter(|c| self.heap.is(*c, Kind::Ctor))
            .and_then(|c| self.heap.field(c, i))
            .ok_or_else(|| Stop::broken(Broken::NotAHandle(v.view())))
    }

    /// The connection a `Socket` or a `TlsSocket` holds.
    fn conn_of(&self, v: Value) -> Result<Handle, Stop> {
        self.handle_of(self.field_of(v, 0)?)
    }

    /// `Socket(conn, peer)`, holding `peer`'s reference.
    fn socket_value(&mut self, conn: Handle, peer: Value) -> Result<Value, Stop> {
        let socket = self.net_types()?.socket;
        let conn = self.handle_value(conn)?;
        self.build(socket, [conn, peer])
    }

    fn ip_value(&mut self, ip: IpAddr) -> Result<Value, Stop> {
        let types = self.address_types()?;
        let form = match ip {
            IpAddr::V4(_) => types.v4,
            IpAddr::V6(_) => types.v6,
        };
        let text = Value::cell(self.heap.string(ip.to_string().as_bytes()).map_err(fault)?);
        self.build(form, [text])
    }

    fn socket_addr_value(&mut self, addr: SocketAddr) -> Result<Value, Stop> {
        let types = self.address_types()?;
        let ip = self.ip_value(addr.ip())?;
        let port = bigint::int(&mut self.heap, addr.port()).map_err(fault)?;
        self.build(types.socket_address, [ip, port])
    }

    /// The address the `SocketAddress` `v` names, or `None` when its port is
    /// not one. `address.socket_address` checks the port, so `None` is only
    /// for a value built some other way.
    fn socket_addr(&self, v: Value) -> Result<Option<SocketAddr>, Stop> {
        let not_an_address = || Stop::broken(Broken::NotASocketAddress(v.view()));
        let ip = self.field_of(v, 0).map_err(|_| not_an_address())?;
        let text = self.utf8_of(self.field_of(ip, 0).map_err(|_| not_an_address())?)?;
        let ip = text.parse::<IpAddr>().map_err(|_| not_an_address())?;
        let port = match self.int_of(self.field_of(v, 1).map_err(|_| not_an_address())?)? {
            Int::Small(n) => u16::try_from(n).ok(),
            Int::Big(_) => None,
        };
        Ok(port.map(|p| SocketAddr::new(ip, p)))
    }

    /// A time from `time.monotonic`, in milliseconds, as a deadline. One
    /// before the run began has passed already.
    fn deadline(&self, v: Value) -> Result<Deadline, Stop> {
        Ok(Deadline(match self.int_of(v)? {
            Int::Small(n) => u128::try_from(n).unwrap_or(0),
            Int::Big(n) if n.sign() == num_bigint::Sign::Minus => 0,
            Int::Big(n) => u128::try_from(n).unwrap_or(u128::MAX),
        }))
    }

    /// How many bytes a read may give. A read gives at least one, so a
    /// `max` below one is one.
    fn read_max(&self, v: Value) -> Result<usize, Stop> {
        Ok(match self.int_of(v)? {
            Int::Small(n) => usize::try_from(n).unwrap_or(0).max(1),
            Int::Big(n) if n.sign() == num_bigint::Sign::Minus => 1,
            Int::Big(_) => usize::MAX,
        })
    }

    /// `socket.Data(bytes)` or `socket.Closed`.
    fn received(&mut self, got: Received) -> Result<Value, Stop> {
        let types = self.net_types()?;
        match got {
            Received::Data(bytes) => {
                if bytes.len() > binary::MAX_BYTES {
                    return Err(Stop::HeapFull);
                }
                let b = Value::cell(binary::from_bytes(&mut self.heap, &bytes).map_err(fault)?);
                self.build(types.data, [b])
            }
            Received::Closed => Ok(types.closed),
        }
    }

    fn invalid_port(&mut self) -> Result<Value, Stop> {
        let invalid = self.net_types()?.invalid_port;
        self.err(invalid)
    }

    /// The `NetError` `e` stands for.
    fn net_error(&mut self, e: NetError) -> Result<Value, Stop> {
        let t = self.net_types()?;
        Ok(match e {
            NetError::TimedOut => t.timed_out,
            NetError::ConnectionRefused => t.connection_refused,
            NetError::ConnectionReset => t.connection_reset,
            NetError::ConnectionAborted => t.connection_aborted,
            NetError::NotConnected => t.not_connected,
            NetError::BrokenPipe => t.broken_pipe,
            NetError::AddrInUse => t.addr_in_use,
            NetError::AddrNotAvailable => t.addr_not_available,
            NetError::NetworkDown => t.network_down,
            NetError::NetworkUnreachable => t.network_unreachable,
            NetError::HostUnreachable => t.host_unreachable,
            NetError::PermissionDenied => t.permission_denied,
            NetError::UnexpectedEof => t.unexpected_eof,
            NetError::Errno(code) => {
                let code = bigint::int(&mut self.heap, code).map_err(fault)?;
                self.build(t.errno, [code])?
            }
        })
    }

    fn net_err(&mut self, e: NetError) -> Result<Value, Stop> {
        let e = self.net_error(e)?;
        self.err(e)
    }

    fn tls_err(&mut self, e: TlsError) -> Result<Value, Stop> {
        let t = self.tls_types()?;
        let error = match e {
            TlsError::CertificateUnknownIssuer => t.certificate_unknown_issuer,
            TlsError::CertificateExpired => t.certificate_expired,
            TlsError::CertificateNotYetValid => t.certificate_not_yet_valid,
            TlsError::CertificateRevoked => t.certificate_revoked,
            TlsError::HostnameMismatch => t.hostname_mismatch,
            TlsError::BadCertificate => t.bad_certificate,
            TlsError::ProtocolError => t.protocol_error,
            TlsError::HandshakeFailed => t.handshake_failed,
            TlsError::InvalidServerName => t.invalid_server_name,
            TlsError::Transport(cause) => {
                let cause = self.net_error(cause)?;
                self.build(t.transport, [cause])?
            }
        };
        self.err(error)
    }
}
