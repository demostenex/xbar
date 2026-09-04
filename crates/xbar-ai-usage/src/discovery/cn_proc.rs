use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, RawFd};

const CN_IDX_PROC: u32 = 1;
const CN_VAL_PROC: u32 = 1;
const PROC_CN_MCAST_LISTEN: u32 = 1;
const PROC_CN_MCAST_IGNORE: u32 = 2;
const PROC_EVENT_NONE: u32 = 0;
const PROC_EVENT_FORK: u32 = 0x0000_0001;
const PROC_EVENT_EXEC: u32 = 0x0000_0002;
const PROC_EVENT_EXIT: u32 = 0x8000_0000;

#[derive(Debug)]
pub enum CnProcError {
    Io(io::Error),
    Ack(u32),
    Malformed,
    Lost,
}
impl fmt::Display for CnProcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "netlink: {e}"),
            Self::Ack(e) => write!(f, "CN_PROC ACK error {e}"),
            Self::Malformed => write!(f, "malformed CN_PROC message"),
            Self::Lost => write!(f, "CN_PROC message loss detected"),
        }
    }
}
impl std::error::Error for CnProcError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelEvent {
    Exec(u32),
    Exit(u32),
}

/// One blocking connector socket. ENOBUFS and datagram truncation become
/// `Lost`; the connector UAPI has no documented reliable per-subscriber
/// sequence, so sequence gaps are not inferred.
pub struct CnProc {
    fd: RawFd,
    pending: VecDeque<KernelEvent>,
}

impl CnProc {
    pub fn listen() -> Result<Self, CnProcError> {
        let fd =
            unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM, libc::NETLINK_CONNECTOR) };
        if fd < 0 {
            return Err(CnProcError::Io(io::Error::last_os_error()));
        }
        let result = (|| {
            let mut address: libc::sockaddr_nl = unsafe { mem::zeroed() };
            address.nl_family = libc::AF_NETLINK as u16;
            address.nl_pid = std::process::id();
            address.nl_groups = CN_IDX_PROC;
            let bound = unsafe {
                libc::bind(
                    fd,
                    (&address as *const libc::sockaddr_nl).cast(),
                    mem::size_of_val(&address) as u32,
                )
            };
            if bound < 0 {
                return Err(CnProcError::Io(io::Error::last_os_error()));
            }
            send_operation(fd, PROC_CN_MCAST_LISTEN)?;
            let mut pending = VecDeque::new();
            loop {
                let mut buffer = [0u8; 8192];
                let length = recv(fd, &mut buffer)?;
                let (ack, events) = parse_messages(&buffer[..length])?;
                pending.extend(events);
                if let Some(err) = ack {
                    if err != 0 {
                        return Err(CnProcError::Ack(err));
                    }
                    break;
                }
            }
            Ok(Self { fd, pending })
        })();
        if result.is_err() {
            unsafe {
                libc::close(fd);
            }
        }
        result
    }
    pub fn fd(&self) -> RawFd {
        self.fd
    }
    pub fn recv_batch(&mut self) -> Result<Vec<KernelEvent>, CnProcError> {
        if !self.pending.is_empty() {
            return Ok(self.pending.drain(..).collect());
        }
        let mut buffer = [0u8; 8192];
        let length = recv(self.fd, &mut buffer)?;
        let (_, events) = parse_messages(&buffer[..length]).map_err(|error| match error {
            CnProcError::Malformed => CnProcError::Lost,
            other => other,
        })?;
        Ok(events)
    }
    pub fn next_event(&mut self) -> Result<KernelEvent, CnProcError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(event);
            }
            let events = self.recv_batch()?;
            self.pending.extend(events);
        }
    }
}
impl AsRawFd for CnProc {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}
impl Drop for CnProc {
    fn drop(&mut self) {
        let _ = send_operation(self.fd, PROC_CN_MCAST_IGNORE);
        unsafe {
            libc::close(self.fd);
        }
    }
}

fn send_operation(fd: RawFd, operation: u32) -> Result<(), CnProcError> {
    let mut buffer = [0u8; 40];
    put_u32(&mut buffer, 0, 40);
    put_u16(&mut buffer, 4, 0x0001);
    put_u32(&mut buffer, 8, 1);
    put_u32(&mut buffer, 16, CN_IDX_PROC);
    put_u32(&mut buffer, 20, CN_VAL_PROC);
    put_u32(&mut buffer, 24, 0);
    put_u32(&mut buffer, 28, 0);
    put_u16(&mut buffer, 32, 4);
    put_u16(&mut buffer, 34, 0);
    put_u32(&mut buffer, 36, operation);
    let mut address: libc::sockaddr_nl = unsafe { mem::zeroed() };
    address.nl_family = libc::AF_NETLINK as u16;
    let sent = unsafe {
        libc::sendto(
            fd,
            buffer.as_ptr().cast(),
            buffer.len(),
            0,
            (&address as *const libc::sockaddr_nl).cast(),
            mem::size_of_val(&address) as u32,
        )
    };
    if sent < 0 {
        Err(CnProcError::Io(io::Error::last_os_error()))
    } else {
        Ok(())
    }
}
fn recv(fd: RawFd, buffer: &mut [u8]) -> Result<usize, CnProcError> {
    let length = unsafe {
        libc::recv(
            fd,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            libc::MSG_TRUNC,
        )
    };
    if length >= 0 && (length as usize) <= buffer.len() {
        return Ok(length as usize);
    }
    if length >= 0 {
        return Err(CnProcError::Lost);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOBUFS) {
        Err(CnProcError::Lost)
    } else {
        Err(CnProcError::Io(error))
    }
}

fn parse_messages(buffer: &[u8]) -> Result<(Option<u32>, Vec<KernelEvent>), CnProcError> {
    let mut offset = 0;
    let mut ack = None;
    let mut events = Vec::new();
    while offset < buffer.len() {
        let remaining = buffer.len() - offset;
        if remaining < 16 {
            return Err(CnProcError::Malformed);
        }
        let length = u32_at(buffer, offset)? as usize;
        if !(16..=remaining).contains(&length) {
            return Err(CnProcError::Malformed);
        }
        let aligned = (length + 3) & !3;
        if aligned > remaining {
            return Err(CnProcError::Malformed);
        }
        let record = &buffer[offset..offset + length];
        let payload = &record[16..];
        if payload.len() < 20 {
            return Err(CnProcError::Malformed);
        }
        let connector_length = u16_at(payload, 16)? as usize;
        if connector_length > payload.len() - 20 {
            return Err(CnProcError::Malformed);
        }
        if u32_at(payload, 0)? != CN_IDX_PROC || u32_at(payload, 4)? != CN_VAL_PROC {
            offset += aligned;
            continue;
        }
        let event = &payload[20..];
        if event.len() < 20 {
            return Err(CnProcError::Malformed);
        }
        match u32_at(event, 0)? {
            PROC_EVENT_NONE => ack = Some(u32_at(event, 16)?),
            PROC_EVENT_EXEC => events.push(KernelEvent::Exec(u32_at(event, 16)?)),
            PROC_EVENT_EXIT => events.push(KernelEvent::Exit(u32_at(event, 16)?)),
            PROC_EVENT_FORK => {}
            _ => {}
        }
        offset += aligned;
    }
    Ok((ack, events))
}
fn u32_at(buffer: &[u8], offset: usize) -> Result<u32, CnProcError> {
    buffer
        .get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_ne_bytes)
        .ok_or(CnProcError::Malformed)
}
fn u16_at(buffer: &[u8], offset: usize) -> Result<u16, CnProcError> {
    buffer
        .get(offset..offset + 2)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_ne_bytes)
        .ok_or(CnProcError::Malformed)
}
fn put_u16(buffer: &mut [u8], offset: usize, value: u16) {
    buffer[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
}
fn put_u32(buffer: &mut [u8], offset: usize, value: u32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    fn message(what: u32, value: u32) -> Vec<u8> {
        let mut b = vec![0u8; 56];
        put_u32(&mut b, 0, 56);
        put_u32(&mut b, 16, CN_IDX_PROC);
        put_u32(&mut b, 20, CN_VAL_PROC);
        put_u16(&mut b, 32, 20);
        put_u32(&mut b, 36, what);
        put_u32(&mut b, 52, value);
        b
    }
    #[test]
    fn valid_ack() {
        assert_eq!(
            parse_messages(&message(PROC_EVENT_NONE, 0)).unwrap().0,
            Some(0)
        );
    }
    #[test]
    fn error_ack() {
        assert_eq!(
            parse_messages(&message(PROC_EVENT_NONE, 5)).unwrap().0,
            Some(5)
        );
    }
    #[test]
    fn exec_and_exit() {
        assert_eq!(
            parse_messages(&message(PROC_EVENT_EXEC, 7)).unwrap().1,
            vec![KernelEvent::Exec(7)]
        );
        assert_eq!(
            parse_messages(&message(PROC_EVENT_EXIT, 8)).unwrap().1,
            vec![KernelEvent::Exit(8)]
        );
    }
    #[test]
    fn multiple_records_and_alignment() {
        let mut b = message(PROC_EVENT_EXEC, 7);
        b.extend_from_slice(&message(PROC_EVENT_EXIT, 8));
        assert_eq!(parse_messages(&b).unwrap().1.len(), 2);
    }
    #[test]
    fn truncated_nlmsg_is_rejected() {
        assert!(matches!(
            parse_messages(&[56, 0, 0]),
            Err(CnProcError::Malformed)
        ));
    }
    #[test]
    fn truncated_cn_message_is_rejected() {
        let mut b = message(PROC_EVENT_EXEC, 7);
        b[0] = 30;
        b.truncate(30);
        assert!(matches!(parse_messages(&b), Err(CnProcError::Malformed)));
    }
    #[test]
    fn unknown_event_is_ignored() {
        assert!(parse_messages(&message(99, 7)).unwrap().1.is_empty());
    }
}
