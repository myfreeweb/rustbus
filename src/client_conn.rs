//! The connection stuff you probably want to use. Conn is the lowlevel abstraction RpcConn is the higher level wrapper with convenience functions
//! over the Conn struct.

use crate::auth;
use crate::message;
use crate::wire::marshal;
use crate::wire::unmarshal;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time;

use nix::cmsg_space;
use nix::sys::socket::recvmsg;
use nix::sys::socket::sendmsg;
use nix::sys::socket::ControlMessage;
use nix::sys::socket::ControlMessageOwned;
use nix::sys::socket::MsgFlags;
use nix::sys::uio::IoVec;

/// Convenience wrapper around the lowlevel connection
pub struct RpcConn<'msga, 'msge> {
    signals: VecDeque<message::Message<'msga, 'msge>>,
    calls: VecDeque<message::Message<'msga, 'msge>>,
    responses: HashMap<u32, message::Message<'msga, 'msge>>,
    conn: Conn,
    filter: Box<MessageFilter>,
}

/// Filter out messages you dont want in your RpcConn.
/// If this filters out a call, the RpcConn will send a UnknownMethod error to the caller. Other messages are just dropped
/// if the filter returns false.
/// ```rust,no_run
/// use rustbus::{get_session_bus_path, standard_messages, Conn, Container, params::DictMap, MessageBuilder, MessageType, RpcConn};
///
/// fn main() -> Result<(), rustbus::client_conn::Error> {
///     let session_path = get_session_bus_path()?;
///     let con = Conn::connect_to_bus(session_path, true)?;
///     let mut rpc_con = RpcConn::new(con);
///
///     rpc_con.set_filter(Box::new(|msg| match msg.typ {
///     MessageType::Call => {
///         let right_interface_object = msg.object.eq(&Some("/io/killing/spark".into()))
///             && msg.interface.eq(&Some("io.killing.spark".into()));
///
///         let right_member = if let Some(member) = &msg.member {
///             member.eq("Echo") || member.eq("Reverse")
///         } else {
///             false
///         };
///         let keep = right_interface_object && right_member;
///         if !keep {
///             println!("Discard: {:?}", msg);
///         }
///         keep
///     }
///     MessageType::Invalid => false,
///     MessageType::Error => true,
///     MessageType::Reply => true,
///     MessageType::Signal => false,
/// }));
///
/// Ok(())
/// }
/// ```
pub type MessageFilter = dyn Fn(&message::Message) -> bool;

impl<'msga, 'msge> RpcConn<'msga, 'msge> {
    pub fn new(conn: Conn) -> Self {
        RpcConn {
            signals: VecDeque::new(),
            calls: VecDeque::new(),
            responses: HashMap::new(),
            conn,
            filter: Box::new(|_| true),
        }
    }

    pub fn conn_mut(&mut self) -> &mut Conn {
        &mut self.conn
    }

    /// get the next new serial
    pub fn alloc_serial(&mut self) -> u32 {
        self.conn.alloc_serial()
    }

    pub fn session_conn(timeout: Option<time::Duration>) -> Result<Self> {
        let session_path = get_session_bus_path()?;
        let con = Conn::connect_to_bus(session_path, true)?;
        let mut con = Self::new(con);
        let serial = con.send_message(&mut crate::standard_messages::hello(), None)?;
        con.wait_response(serial, timeout)?;
        Ok(con)
    }

    pub fn system_conn(timeout: Option<time::Duration>) -> Result<Self> {
        let session_path = get_system_bus_path()?;
        let con = Conn::connect_to_bus(session_path, true)?;
        let mut con = Self::new(con);
        let serial = con.send_message(&mut crate::standard_messages::hello(), None)?;
        con.wait_response(serial, timeout)?;
        Ok(con)
    }

    pub fn set_filter(&mut self, filter: Box<MessageFilter>) {
        self.filter = filter;
    }

    /// Return a response if one is there but dont block
    pub fn try_get_response(&mut self, serial: u32) -> Option<message::Message<'msga, 'msge>> {
        self.responses.remove(&serial)
    }

    /// Return a response if one is there or block until it arrives
    pub fn wait_response(
        &mut self,
        serial: u32,
        timeout: Option<time::Duration>,
    ) -> Result<message::Message<'msga, 'msge>> {
        loop {
            if let Some(msg) = self.try_get_response(serial) {
                return Ok(msg);
            }
            self.refill(timeout)?;
        }
    }

    /// Return a signal if one is there but dont block
    pub fn try_get_signal(&mut self) -> Option<message::Message<'msga, 'msge>> {
        self.signals.pop_front()
    }

    /// Return a sginal if one is there or block until it arrives
    pub fn wait_signal(
        &mut self,
        timeout: Option<time::Duration>,
    ) -> Result<message::Message<'msga, 'msge>> {
        loop {
            if let Some(msg) = self.try_get_signal() {
                return Ok(msg);
            }
            self.refill(timeout)?;
        }
    }

    /// Return a call if one is there but dont block
    pub fn try_get_call(&mut self) -> Option<message::Message<'msga, 'msge>> {
        self.calls.pop_front()
    }

    /// Return a call if one is there or block until it arrives
    pub fn wait_call(
        &mut self,
        timeout: Option<time::Duration>,
    ) -> Result<message::Message<'msga, 'msge>> {
        loop {
            if let Some(msg) = self.try_get_call() {
                return Ok(msg);
            }
            self.refill(timeout)?;
        }
    }

    /// Send a message to the bus
    pub fn send_message(
        &mut self,
        msg: &mut message::Message<'msga, 'msge>,
        timeout: Option<time::Duration>,
    ) -> Result<u32> {
        self.conn.send_message(msg, timeout)
    }

    /// This blocks until a new message (that should not be ignored) arrives.
    /// The message gets placed into the correct list
    fn refill(&mut self, timeout: Option<time::Duration>) -> Result<()> {
        let start_time = time::Instant::now();
        loop {
            let msg = self
                .conn
                .get_next_message(calc_timeout_left(&start_time, timeout)?)?;

            if self.filter.as_ref()(&msg) {
                match msg.typ {
                    message::MessageType::Call => {
                        self.calls.push_back(msg);
                    }
                    message::MessageType::Invalid => return Err(Error::UnexpectedTypeReceived),
                    message::MessageType::Error => {
                        self.responses.insert(msg.response_serial.unwrap(), msg);
                    }
                    message::MessageType::Reply => {
                        self.responses.insert(msg.response_serial.unwrap(), msg);
                    }
                    message::MessageType::Signal => {
                        self.signals.push_back(msg);
                    }
                }
                break;
            } else {
                match msg.typ {
                    message::MessageType::Call => {
                        let mut reply = crate::standard_messages::unknown_method(&msg);
                        self.conn
                            .send_message(&mut reply, calc_timeout_left(&start_time, timeout)?)?;
                    }
                    message::MessageType::Invalid => return Err(Error::UnexpectedTypeReceived),
                    message::MessageType::Error => {
                        // just drop it
                    }
                    message::MessageType::Reply => {
                        // just drop it
                    }
                    message::MessageType::Signal => {
                        // just drop it
                    }
                }
            }
        }
        Ok(())
    }
}

/// A lowlevel abstraction over the raw unix socket
#[derive(Debug)]
pub struct Conn {
    socket_path: PathBuf,
    stream: UnixStream,

    byteorder: message::ByteOrder,

    msg_buf_in: Vec<u8>,
    cmsgs_in: Vec<ControlMessageOwned>,

    msg_buf_out: Vec<u8>,

    serial_counter: u32,
}

/// Errors that can occur when using the Conn/RpcConn
#[derive(Debug)]
pub enum Error {
    IoError(std::io::Error),
    NixError(nix::Error),
    UnmarshalError(unmarshal::Error),
    MarshalError(message::Error),
    AuthFailed,
    UnixFdNegotiationFailed,
    NameTaken,
    AddressTypeNotSupported(String),
    PathDoesNotExist(String),
    NoAdressFound,
    UnexpectedTypeReceived,
    TimedOut,
}

impl std::convert::From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::IoError(e)
    }
}

impl std::convert::From<unmarshal::Error> for Error {
    fn from(e: unmarshal::Error) -> Error {
        Error::UnmarshalError(e)
    }
}
impl std::convert::From<message::Error> for Error {
    fn from(e: message::Error) -> Error {
        Error::MarshalError(e)
    }
}
impl std::convert::From<nix::Error> for Error {
    fn from(e: nix::Error) -> Error {
        Error::NixError(e)
    }
}

type Result<T> = std::result::Result<T, Error>;

impl<'msga, 'msge> Conn {
    /// Connect to a unix socket and choose a byteorder
    pub fn connect_to_bus_with_byteorder(
        path: PathBuf,
        byteorder: message::ByteOrder,
        with_unix_fd: bool,
    ) -> Result<Conn> {
        let mut stream = UnixStream::connect(&path)?;
        match auth::do_auth(&mut stream)? {
            auth::AuthResult::Ok => {}
            auth::AuthResult::Rejected => return Err(Error::AuthFailed),
        }

        if with_unix_fd {
            match auth::negotiate_unix_fds(&mut stream)? {
                auth::AuthResult::Ok => {}
                auth::AuthResult::Rejected => return Err(Error::UnixFdNegotiationFailed),
            }
        }

        auth::send_begin(&mut stream)?;

        Ok(Conn {
            socket_path: path,
            stream,
            msg_buf_in: Vec::new(),
            cmsgs_in: Vec::new(),
            msg_buf_out: Vec::new(),
            byteorder,

            serial_counter: 1,
        })
    }

    pub fn can_read_from_source(&self) -> nix::Result<bool> {
        let mut fdset = nix::sys::select::FdSet::new();
        let fd = self.stream.as_raw_fd();
        fdset.insert(fd);

        use nix::sys::time::TimeValLike;
        let mut zero_timeout = nix::sys::time::TimeVal::microseconds(0);

        nix::sys::select::select(None, Some(&mut fdset), None, None, Some(&mut zero_timeout))?;
        Ok(fdset.contains(fd))
    }

    /// Connect to a unix socket. The default little endian byteorder is used
    pub fn connect_to_bus(path: PathBuf, with_unix_fd: bool) -> Result<Conn> {
        Self::connect_to_bus_with_byteorder(path, message::ByteOrder::LittleEndian, with_unix_fd)
    }

    /// Reads from the source once but takes care that the internal buffer only reaches at maximum max_buffer_size
    /// so we can process messages separatly and avoid leaking file descriptors to wrong messages
    fn refill_buffer(
        &mut self,
        max_buffer_size: usize,
        timeout: Option<time::Duration>,
    ) -> Result<()> {
        let bytes_to_read = max_buffer_size - self.msg_buf_in.len();

        const BUFSIZE: usize = 512;
        let mut tmpbuf = [0u8; BUFSIZE];
        let iovec = IoVec::from_mut_slice(&mut tmpbuf[..usize::min(bytes_to_read, BUFSIZE)]);

        let mut cmsgspace = cmsg_space!([RawFd; 10]);
        let flags = MsgFlags::empty();

        let old_timeout = self.stream.read_timeout()?;
        self.stream.set_read_timeout(timeout)?;
        let msg = recvmsg(
            self.stream.as_raw_fd(),
            &[iovec],
            Some(&mut cmsgspace),
            flags,
        )
        .map_err(|e| match e.as_errno() {
            Some(nix::errno::Errno::EAGAIN) => Error::TimedOut,
            _ => Error::NixError(e),
        })?;
        self.stream.set_read_timeout(old_timeout)?;
        self.msg_buf_in
            .extend(&mut tmpbuf[..msg.bytes].iter().copied());
        self.cmsgs_in.extend(msg.cmsgs());
        Ok(())
    }

    pub fn bytes_needed_for_current_message(&self) -> Result<usize> {
        if self.msg_buf_in.len() < 16 {
            return Ok(16);
        }
        let (_, header) = unmarshal::unmarshal_header(&self.msg_buf_in, 0)?;
        let (_, header_fields_len) = crate::wire::util::parse_u32(
            &self.msg_buf_in[unmarshal::HEADER_LEN..],
            header.byteorder,
        )?;
        let complete_header_size = unmarshal::HEADER_LEN + header_fields_len as usize + 4; // +4 because the length of the header fields does not count

        let padding_between_header_and_body = 8 - ((complete_header_size) % 8);
        let padding_between_header_and_body = if padding_between_header_and_body == 8 {
            0
        } else {
            padding_between_header_and_body
        };

        let bytes_needed = complete_header_size as usize
            + padding_between_header_and_body
            + header.body_len as usize;
        Ok(bytes_needed)
    }

    // Checks if the internal buffer currently holds a complete message
    pub fn buffer_contains_whole_message(&self) -> Result<bool> {
        if self.msg_buf_in.len() < 16 {
            return Ok(false);
        }
        let bytes_needed = self.bytes_needed_for_current_message();
        match bytes_needed {
            Err(e) => {
                if let Error::UnmarshalError(unmarshal::Error::NotEnoughBytes) = e {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
            Ok(bytes_needed) => Ok(self.msg_buf_in.len() >= bytes_needed),
        }
    }
    /// Blocks until a message has been read from the conn or the timeout has been reached
    pub fn read_whole_message(&mut self, timeout: Option<time::Duration>) -> Result<()> {
        // This whole dance around reading exact amounts of bytes is necessary to read messages exactly at their bounds.
        // I think thats necessary so we can later add support for unixfd sending
        //calc timeout in reference to this point in time
        let start_time = time::Instant::now();

        while !self.buffer_contains_whole_message()? {
            self.refill_buffer(
                self.bytes_needed_for_current_message()?,
                calc_timeout_left(&start_time, timeout)?,
            )?;
        }
        Ok(())
    }

    /// Blocks until one read towards the message has been performed from the conn or the timeout has been reached
    pub fn read_once(&mut self, timeout: Option<time::Duration>) -> Result<()> {
        self.refill_buffer(self.bytes_needed_for_current_message()?, timeout)?;
        Ok(())
    }

    /// Blocks until a message has been read from the conn or the timeout has been reached
    pub fn get_next_message(
        &mut self,
        timeout: Option<time::Duration>,
    ) -> Result<message::Message<'msga, 'msge>> {
        self.read_whole_message(timeout)?;
        let (_, header) = unmarshal::unmarshal_header(&self.msg_buf_in, 0)?;
        let (bytes_used, mut msg) =
            unmarshal::unmarshal_next_message(&header, &self.msg_buf_in, unmarshal::HEADER_LEN)?;
        if self.msg_buf_in.len() != bytes_used + unmarshal::HEADER_LEN {
            return Err(Error::UnmarshalError(unmarshal::Error::NotAllBytesUsed));
        }
        self.msg_buf_in.clear();

        for cmsg in &self.cmsgs_in {
            match cmsg {
                ControlMessageOwned::ScmRights(fds) => {
                    msg.raw_fds.extend(fds);
                }
                _ => {
                    // TODO what to do?
                    println!("Cmsg other than ScmRights: {:?}", cmsg);
                }
            }
        }
        self.cmsgs_in.clear();
        Ok(msg)
    }

    /// get the next new serial
    pub fn alloc_serial(&mut self) -> u32 {
        let serial = self.serial_counter;
        self.serial_counter += 1;
        serial
    }

    /// send a message over the conn
    pub fn send_message(
        &mut self,
        msg: &mut message::Message<'msga, 'msge>,
        timeout: Option<time::Duration>,
    ) -> Result<u32> {
        self.msg_buf_out.clear();
        let (remove_later, serial) = if let Some(serial) = msg.serial {
            (false, serial)
        } else {
            let serial = self.serial_counter;
            self.serial_counter += 1;
            msg.serial = Some(serial);
            (true, serial)
        };

        marshal::marshal(
            &msg,
            message::ByteOrder::LittleEndian,
            &[],
            &mut self.msg_buf_out,
        )?;

        let iov = [IoVec::from_slice(&self.msg_buf_out)];
        let flags = MsgFlags::empty();

        let old_timeout = self.stream.read_timeout()?;
        self.stream.set_read_timeout(timeout)?;
        let l = sendmsg(
            self.stream.as_raw_fd(),
            &iov,
            &[ControlMessage::ScmRights(&msg.raw_fds)],
            flags,
            None,
        )?;
        self.stream.set_read_timeout(old_timeout)?;
        assert_eq!(l, self.msg_buf_out.len());

        if remove_later {
            msg.serial = None;
        }

        Ok(serial)
    }
}

/// Convenience function that returns a path to the session bus according to the env var $DBUS_SESSION_BUS_ADDRESS
pub fn get_session_bus_path() -> Result<PathBuf> {
    if let Ok(envvar) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        if envvar.starts_with("unix:path=") {
            let ps = envvar.trim_start_matches("unix:path=");
            let p = PathBuf::from(&ps);
            if p.exists() {
                Ok(p)
            } else {
                Err(Error::PathDoesNotExist(ps.to_owned()))
            }
        } else {
            Err(Error::AddressTypeNotSupported(envvar))
        }
    } else {
        Err(Error::NoAdressFound)
    }
}

/// Convenience function that returns a path to the system bus at /run/dbus/systemd_bus_socket
pub fn get_system_bus_path() -> Result<PathBuf> {
    let ps = "/run/dbus/system_bus_socket";
    let p = PathBuf::from(&ps);
    if p.exists() {
        Ok(p)
    } else {
        Err(Error::PathDoesNotExist(ps.to_owned()))
    }
}

fn calc_timeout_left(
    start_time: &time::Instant,
    timeout: Option<time::Duration>,
) -> Result<Option<time::Duration>> {
    match timeout {
        Some(timeout) => {
            let elapsed = start_time.elapsed();
            if elapsed >= timeout {
                return Err(Error::TimedOut);
            }
            let time_left = timeout - elapsed;
            Ok(Some(time_left))
        }
        None => Ok(None),
    }
}
