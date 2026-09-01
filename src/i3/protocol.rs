use std::io;

pub const MAGIC: &[u8; 6] = b"i3-ipc";
pub const GET_WORKSPACES: u32 = 1;
pub const SUBSCRIBE: u32 = 2;
pub const GET_TREE: u32 = 4;
pub const EVENT_WORKSPACE: u32 = 0x8000_0000;
pub const EVENT_OUTPUT: u32 = 0x8000_0001;
pub const EVENT_WINDOW: u32 = 0x8000_0003;

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub message_type: u32,
    pub payload: Vec<u8>,
}

pub fn encode(message_type: u32, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.extend_from_slice(MAGIC);
    frame.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    frame.extend_from_slice(&message_type.to_ne_bytes());
    frame.extend_from_slice(payload);
    frame
}

#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}
impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> io::Result<Vec<Frame>> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            if self.buffer.len() < 14 {
                break;
            }
            if &self.buffer[..6] != MAGIC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid i3 IPC magic",
                ));
            }
            let len = u32::from_ne_bytes(self.buffer[6..10].try_into().unwrap()) as usize;
            if self.buffer.len() < 14 + len {
                break;
            }
            let ty = u32::from_ne_bytes(self.buffer[10..14].try_into().unwrap());
            frames.push(Frame {
                message_type: ty,
                payload: self.buffer[14..14 + len].to_vec(),
            });
            self.buffer.drain(..14 + len);
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encodes_header() {
        let x = encode(GET_WORKSPACES, b"{}");
        assert_eq!(&x[..6], MAGIC);
        assert_eq!(&x[6..10], &(2u32.to_ne_bytes()));
        assert_eq!(&x[10..14], &(GET_WORKSPACES.to_ne_bytes()));
    }
    #[test]
    fn fragmented_and_concatenated_frames() {
        let a = encode(EVENT_WINDOW, b"a");
        let b = encode(EVENT_OUTPUT, b"bc");
        let mut d = Decoder::default();
        assert!(d.push(&a[..5]).unwrap().is_empty());
        assert!(d.push(&a[5..]).unwrap().len() == 1);
        let all = [b.as_slice(), a.as_slice()].concat();
        let frames = d.push(&all).unwrap();
        assert_eq!(
            frames,
            vec![
                Frame {
                    message_type: EVENT_OUTPUT,
                    payload: b"bc".to_vec()
                },
                Frame {
                    message_type: EVENT_WINDOW,
                    payload: b"a".to_vec()
                }
            ]
        );
    }
    #[test]
    fn partial_payload_waits() {
        let x = encode(1, b"payload");
        let mut d = Decoder::default();
        assert!(d.push(&x[..16]).unwrap().is_empty());
        assert_eq!(d.push(&x[16..]).unwrap()[0].payload, b"payload");
    }
}
