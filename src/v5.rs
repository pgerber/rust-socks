use byteorder::{ReadBytesExt, WriteBytesExt, BigEndian};
use std::cmp;
use std::io::{self, Read, Write, BufReader};
use std::net::{SocketAddr, ToSocketAddrs, SocketAddrV4, SocketAddrV6, TcpStream, Ipv4Addr,
               Ipv6Addr, UdpSocket};
use std::ptr;

use {ToTargetAddr, TargetAddr};
use writev::WritevExt;

const MAX_ADDR_LEN: usize = 260;

fn read_addr<R: Read>(socket: &mut R) -> io::Result<TargetAddr> {
    match socket.read_u8()? {
        1 => {
            let ip = Ipv4Addr::from(socket.read_u32::<BigEndian>()?);
            let port = socket.read_u16::<BigEndian>()?;
            Ok(TargetAddr::Ip(SocketAddr::V4(SocketAddrV4::new(ip, port))))
        }
        3 => {
            let len = socket.read_u8()?;
            let mut domain = vec![0; len as usize];
            socket.read_exact(&mut domain)?;
            let domain = String::from_utf8(domain)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let port = socket.read_u16::<BigEndian>()?;
            Ok(TargetAddr::Domain(domain, port))
        }
        4 => {
            let mut ip = [0; 16];
            socket.read_exact(&mut ip)?;
            let ip = Ipv6Addr::from(ip);
            let port = socket.read_u16::<BigEndian>()?;
            Ok(TargetAddr::Ip(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0))))
        }
        _ => Err(io::Error::new(io::ErrorKind::Other, "unsupported address type")),
    }
}

fn read_response(socket: &mut TcpStream) -> io::Result<TargetAddr> {
    let mut socket = BufReader::with_capacity(MAX_ADDR_LEN + 3, socket);

    if socket.read_u8()? != 5 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid response version"));
    }

    match socket.read_u8()? {
        0 => {}
        1 => return Err(io::Error::new(io::ErrorKind::Other, "general SOCKS server failure")),
        2 => return Err(io::Error::new(io::ErrorKind::Other, "connection not allowed by ruleset")),
        3 => return Err(io::Error::new(io::ErrorKind::Other, "network unreachable")),
        4 => return Err(io::Error::new(io::ErrorKind::Other, "host unreachable")),
        5 => return Err(io::Error::new(io::ErrorKind::Other, "connection refused")),
        6 => return Err(io::Error::new(io::ErrorKind::Other, "TTL expired")),
        7 => return Err(io::Error::new(io::ErrorKind::Other, "command not supported")),
        8 => return Err(io::Error::new(io::ErrorKind::Other, "address kind not supported")),
        _ => return Err(io::Error::new(io::ErrorKind::Other, "unknown error")),
    }

    if socket.read_u8()? != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid reserved byte"));
    }

    read_addr(&mut socket)
}

fn write_addr(mut packet: &mut [u8], target: &TargetAddr) -> io::Result<usize> {
    let start_len = packet.len();
    match *target {
        TargetAddr::Ip(SocketAddr::V4(addr)) => {
            packet.write_u8(1).unwrap();
            packet.write_u32::<BigEndian>((*addr.ip()).into()).unwrap();
            packet.write_u16::<BigEndian>(addr.port()).unwrap();
        }
        TargetAddr::Ip(SocketAddr::V6(addr)) => {
            packet.write_u8(4).unwrap();
            packet.write_all(&addr.ip().octets()).unwrap();
            packet.write_u16::<BigEndian>(addr.port()).unwrap();
        }
        TargetAddr::Domain(ref domain, port) => {
            packet.write_u8(3).unwrap();
            if domain.len() > u8::max_value() as usize {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "domain name too long"));
            }
            packet.write_u8(domain.len() as u8).unwrap();
            packet.write_all(domain.as_bytes()).unwrap();
            packet.write_u16::<BigEndian>(port).unwrap();
        }
    }

    Ok(start_len - packet.len())
}

/// A SOCKS5 client.
#[derive(Debug)]
pub struct Socks5Stream {
    socket: TcpStream,
    proxy_addr: TargetAddr,
}

impl Socks5Stream {
    /// Connects to a target server through a SOCKS5 proxy.
    pub fn connect<T, U>(proxy: T, target: U) -> io::Result<Socks5Stream>
        where T: ToSocketAddrs,
              U: ToTargetAddr
    {
        Self::connect_raw(1, proxy, target)
    }

    fn connect_raw<T, U>(command: u8, proxy: T, target: U) -> io::Result<Socks5Stream>
        where T: ToSocketAddrs,
              U: ToTargetAddr
    {
        let mut socket = TcpStream::connect(proxy)?;

        let target = target.to_target_addr()?;

        let packet = [
            5, // protocol version
            1, // method count
            0, // no authentication
        ];
        socket.write_all(&packet)?;

        let mut buf = [0; 2];
        socket.read_exact(&mut buf)?;

        if buf[0] != 5 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid response version"));
        }

        match buf[1] {
            0 => {}
            0xff => return Err(io::Error::new(io::ErrorKind::Other, "no acceptable auth methods")),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown auth method")),
        }

        let mut packet = [0; MAX_ADDR_LEN + 3];
        packet[0] = 5; // protocol version
        packet[1] = command; // command
        packet[2] = 0; // reserved
        let len = write_addr(&mut packet[3..], &target)?;
        socket.write_all(&packet[..len + 3])?;

        let proxy_addr = read_response(&mut socket)?;

        Ok(Socks5Stream {
            socket: socket,
            proxy_addr: proxy_addr,
        })
    }

    /// Returns the proxy-side address of the connection between the proxy and
    /// target server.
    pub fn proxy_addr(&self) -> &TargetAddr {
        &self.proxy_addr
    }

    /// Returns a shared reference to the inner `TcpStream`.
    pub fn get_ref(&self) -> &TcpStream {
        &self.socket
    }

    /// Returns a mutable reference to the inner `TcpStream`.
    pub fn get_mut(&mut self) -> &mut TcpStream {
        &mut self.socket
    }

    /// Consumes the `Socks5Stream`, returning the inner `TcpStream`.
    pub fn into_inner(self) -> TcpStream {
        self.socket
    }
}

impl Read for Socks5Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket.read(buf)
    }
}

impl<'a> Read for &'a Socks5Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&self.socket).read(buf)
    }
}

impl Write for Socks5Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.socket.flush()
    }
}

impl<'a> Write for &'a Socks5Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&self.socket).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.socket).flush()
    }
}

/// A SOCKS5 BIND client.
#[derive(Debug)]
pub struct Socks5Listener(Socks5Stream);

impl Socks5Listener {
    /// Initiates a BIND request to the specified proxy.
    ///
    /// The proxy will filter incoming connections based on the value of
    /// `target`.
    pub fn bind<T, U>(proxy: T, target: U) -> io::Result<Socks5Listener>
        where T: ToSocketAddrs,
              U: ToTargetAddr
    {
        Socks5Stream::connect_raw(2, proxy, target).map(Socks5Listener)
    }

    /// The address of the proxy-side TCP listener.
    ///
    /// This should be forwarded to the remote process, which should open a
    /// connection to it.
    pub fn proxy_addr(&self) -> &TargetAddr {
        &self.0.proxy_addr
    }

    /// Waits for the remote process to connect to the proxy server.
    ///
    /// The value of `proxy_addr` should be forwarded to the remote process
    /// before this method is called.
    pub fn accept(mut self) -> io::Result<Socks5Stream> {
        self.0.proxy_addr = read_response(&mut self.0.socket)?;
        Ok(self.0)
    }
}

/// A SOCKS5 UDP client.
#[derive(Debug)]
pub struct Socks5Datagram {
    socket: UdpSocket,
    // keeps the session alive
    stream: Socks5Stream,
}

impl Socks5Datagram {
    /// Creates a UDP socket bound to the specified address which will have its
    /// traffic routed through the specified proxy.
    pub fn bind<T, U>(proxy: T, addr: U) -> io::Result<Socks5Datagram>
        where T: ToSocketAddrs,
              U: ToSocketAddrs
    {
        // we don't know what our IP is from the perspective of the proxy, so
        // don't try to pass `addr` in here.
        let dst = TargetAddr::Ip(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)));
        let stream = Socks5Stream::connect_raw(3, proxy, dst)?;

        let socket = UdpSocket::bind(addr)?;
        socket.connect(&stream.proxy_addr)?;

        Ok(Socks5Datagram {
            socket: socket,
            stream: stream,
        })
    }

    /// Like `UdpSocket::send_to`.
    ///
    /// # Note
    ///
    /// The SOCKS protocol inserts a header at the beginning of the message. The
    /// header will be 10 bytes for an IPv4 address, 22 bytes for an IPv6
    /// address, and 7 bytes plus the length of the domain for a domain address.
    pub fn send_to<A>(&self, buf: &[u8], addr: A) -> io::Result<usize>
        where A: ToTargetAddr
    {
        let addr = addr.to_target_addr()?;

        let mut header = [0; MAX_ADDR_LEN + 3];
        // first two bytes are reserved at 0
        // third byte is the fragment id at 0
        let len = write_addr(&mut header[3..], &addr)?;

        self.socket.writev([&header[..len + 3], buf])
    }

    /// Like `UdpSocket::recv_from`.
    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, TargetAddr)> {
        let mut header = [0; MAX_ADDR_LEN + 3];
        let len = self.socket.readv([&mut header, buf])?;

        let overflow = len.saturating_sub(header.len());

        let header_len = cmp::min(header.len(), len);
        let mut header = &mut &header[..header_len];

        if header.read_u16::<BigEndian>()? != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid reserved bytes"));
        }
        if header.read_u8()? != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid fragment id"));
        }
        let addr = read_addr(&mut header)?;

        unsafe {
            ptr::copy(buf.as_ptr(), buf.as_mut_ptr().offset(header.len() as isize), overflow);
        }
        buf[..header.len()].copy_from_slice(header);

        Ok((header.len() + overflow, addr))
    }

    /// Returns the address of the proxy-side UDP socket through which all
    /// messages will be routed.
    pub fn proxy_addr(&self) -> &TargetAddr {
        &self.stream.proxy_addr
    }

    /// Returns a shared reference to the inner socket.
    pub fn get_ref(&self) -> &UdpSocket {
        &self.socket
    }

    /// Returns a mutable reference to the inner socket.
    pub fn get_mut(&mut self) -> &mut UdpSocket {
        &mut self.socket
    }
}

#[cfg(test)]
mod test {
    use std::io::{Read, Write};
    use std::net::{ToSocketAddrs, TcpStream, UdpSocket};

    use super::*;

    #[test]
    fn google() {
        let addr = "google.com:80".to_socket_addrs().unwrap().next().unwrap();
        let mut socket = Socks5Stream::connect("127.0.0.1:1080", addr).unwrap();

        socket.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
        let mut result = vec![];
        socket.read_to_end(&mut result).unwrap();

        println!("{}", String::from_utf8_lossy(&result));
        assert!(result.starts_with(b"HTTP/1.0"));
        assert!(result.ends_with(b"</HTML>\r\n") || result.ends_with(b"</html>"));
    }

    #[test]
    fn google_dns() {
        let mut socket = Socks5Stream::connect("127.0.0.1:1080", "google.com:80").unwrap();

        socket.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
        let mut result = vec![];
        socket.read_to_end(&mut result).unwrap();

        println!("{}", String::from_utf8_lossy(&result));
        assert!(result.starts_with(b"HTTP/1.0"));
        assert!(result.ends_with(b"</HTML>\r\n") || result.ends_with(b"</html>"));
    }

    #[test]
    fn bind() {
        // First figure out our local address that we'll be connecting from
        let socket = Socks5Stream::connect("127.0.0.1:1080", "google.com:80").unwrap();
        let addr = socket.proxy_addr().clone();

        let listener = Socks5Listener::bind("127.0.0.1:1080", addr).unwrap();
        let addr = listener.proxy_addr().clone();
        let mut end = TcpStream::connect(addr).unwrap();
        let mut conn = listener.accept().unwrap();
        conn.write_all(b"hello world").unwrap();
        drop(conn);
        let mut result = vec![];
        end.read_to_end(&mut result).unwrap();
        assert_eq!(result, b"hello world");
    }

    #[test]
    fn associate() {
        let socks = Socks5Datagram::bind("127.0.0.1:1080", "127.0.0.1:15410").unwrap();
        let socket_addr = "127.0.0.1:15411";
        let socket = UdpSocket::bind(socket_addr).unwrap();

        socks.send_to(b"hello world!", socket_addr).unwrap();
        let mut buf = [0; 13];
        let (len, addr) = socket.recv_from(&mut buf).unwrap();
        assert_eq!(len, 12);
        assert_eq!(&buf[..12], b"hello world!");

        socket.send_to(b"hello world!", addr).unwrap();

        let len = socks.recv_from(&mut buf).unwrap().0;
        assert_eq!(len, 12);
        assert_eq!(&buf[..12], b"hello world!");
    }

    #[test]
    fn associate_long() {
        let socks = Socks5Datagram::bind("127.0.0.1:1080", "127.0.0.1:15412").unwrap();
        let socket_addr = "127.0.0.1:15413";
        let socket = UdpSocket::bind(socket_addr).unwrap();

        let mut msg = vec![];
        for i in 0..(MAX_ADDR_LEN + 100) {
            msg.push(i as u8);
        }

        socks.send_to(&msg, socket_addr).unwrap();
        let mut buf = vec![0; msg.len() + 1];
        let (len, addr) = socket.recv_from(&mut buf).unwrap();
        assert_eq!(len, msg.len());
        assert_eq!(msg, &buf[..msg.len()]);

        socket.send_to(&msg, addr).unwrap();

        let mut buf = vec![0; msg.len() + 1];
        let len = socks.recv_from(&mut buf).unwrap().0;
        assert_eq!(len, msg.len());
        assert_eq!(msg, &buf[..msg.len()]);
    }
}
