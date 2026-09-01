pub mod ipc;
pub mod protocol;

pub use ipc::I3Client;
use std::error::Error;
use std::path::PathBuf;
use x11rb::protocol::xproto::AtomEnum;
use x11rb::protocol::xproto::ConnectionExt;

pub fn socket_path(x11: &crate::platform::x11::X11Platform) -> Result<PathBuf, Box<dyn Error>> {
    if let Ok(path) = std::env::var("I3SOCK") {
        return Ok(path.into());
    }
    let atom = x11
        .connection()
        .intern_atom(false, b"I3_SOCKET_PATH")?
        .reply()?
        .atom;
    let root = x11.root();
    let property = x11
        .connection()
        .get_property(false, root, atom, AtomEnum::STRING, 0, u32::MAX)?
        .reply()?;
    let value =
        String::from_utf8(property.value).map_err(|_| "I3_SOCKET_PATH is not valid UTF-8")?;
    if value.is_empty() {
        Err("I3SOCK is not set and I3_SOCKET_PATH is empty".into())
    } else {
        Ok(value.into())
    }
}
