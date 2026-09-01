use super::protocol::{
    self, Decoder, Frame, EVENT_OUTPUT, EVENT_WINDOW, EVENT_WORKSPACE, GET_TREE,
};
use crate::core::{Event, WindowId, WorkspaceState};
use std::error::Error;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

pub struct I3Client {
    stream: UnixStream,
    decoder: Decoder,
}
impl I3Client {
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            stream: UnixStream::connect(path)?,
            decoder: Decoder::default(),
        })
    }
    pub fn raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
    fn send(&mut self, ty: u32, payload: &[u8]) -> Result<(), Box<dyn Error>> {
        self.stream.write_all(&protocol::encode(ty, payload))?;
        Ok(())
    }
    pub fn request_workspaces(&mut self) -> Result<(), Box<dyn Error>> {
        self.send(protocol::GET_WORKSPACES, b"")
    }
    pub fn subscribe(&mut self) -> Result<(), Box<dyn Error>> {
        self.send(protocol::SUBSCRIBE, br#"["workspace","window","output"]"#)
    }
    pub fn request_focused_window(&mut self) -> Result<(), Box<dyn Error>> {
        self.send(GET_TREE, b"")
    }
    pub fn read_events(&mut self) -> Result<Vec<Event>, Box<dyn Error>> {
        let mut buf = [0u8; 8192];
        let n = self.stream.read(&mut buf)?;
        if n == 0 {
            return Err("i3 IPC socket closed".into());
        }
        let frames = self.decoder.push(&buf[..n])?;
        frames
            .into_iter()
            .filter_map(|frame| decode_frame(frame).transpose())
            .collect()
    }
}

fn decode_frame(frame: Frame) -> Result<Option<Event>, Box<dyn Error>> {
    let json: serde_json::Value = serde_json::from_slice(&frame.payload)?;
    match frame.message_type {
        1 => Ok(Some(Event::WorkspacesSnapshot(parse_workspaces(&json)?))),
        2 => Ok(None),
        GET_TREE => {
            let (window, app_name) = find_focused_window_info(&json);
            Ok(Some(Event::WindowFocusedWithApp { window, app_name }))
        }
        EVENT_WORKSPACE => Ok(Some(Event::WorkspaceFocused {
            name: json
                .get("current")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        })),
        EVENT_WINDOW => {
            let change = json.get("change").and_then(|v| v.as_str()).unwrap_or("");
            if change == "focus" {
                let container = json.get("container");
                Ok(Some(Event::WindowFocusedWithApp {
                    window: container
                        .and_then(|v| v.get("window"))
                        .and_then(|v| v.as_u64())
                        .map(|v| WindowId(v as u32)),
                    app_name: container.and_then(application_name),
                }))
            } else {
                Ok(None)
            }
        }
        EVENT_OUTPUT => Ok(Some(Event::X11(
            crate::platform::x11::X11Event::RandrChanged,
        ))),
        _ => Err(format!("unsupported i3 IPC message type {}", frame.message_type).into()),
    }
}

fn find_focused_window_info(value: &serde_json::Value) -> (Option<WindowId>, Option<String>) {
    if value.get("focused").and_then(|focused| focused.as_bool()) == Some(true) {
        if let Some(window) = value.get("window").and_then(|window| window.as_u64()) {
            return (Some(WindowId(window as u32)), application_name(value));
        }
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = value.get(key).and_then(|children| children.as_array()) {
            for child in children {
                let (window, title) = find_focused_window_info(child);
                if window.is_some() {
                    return (window, title);
                }
            }
        }
    }
    (None, None)
}

fn application_name(value: &serde_json::Value) -> Option<String> {
    let properties = value.get("window_properties").unwrap_or(value);
    let class = properties
        .get("class")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let instance = properties
        .get("instance")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let raw = if class.is_empty() { instance } else { class };
    if raw.is_empty() {
        None
    } else if raw.eq_ignore_ascii_case("navigator") && instance.eq_ignore_ascii_case("zen") {
        Some("Zen Browser".into())
    } else {
        Some(raw.to_owned())
    }
}

fn parse_workspaces(value: &serde_json::Value) -> Result<Vec<WorkspaceState>, Box<dyn Error>> {
    let list = value
        .as_array()
        .ok_or("i3 workspaces reply is not an array")?;
    Ok(list
        .iter()
        .map(|v| WorkspaceState {
            name: v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_owned(),
            output: v.get("output").and_then(|x| x.as_str()).map(str::to_owned),
            focused: v.get("focused").and_then(|x| x.as_bool()).unwrap_or(false),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_workspace_snapshot() {
        let frame = Frame {
            message_type: protocol::GET_WORKSPACES,
            payload: br#"[{"name":"1","output":"HDMI-1","focused":true}]"#.to_vec(),
        };
        let event = decode_frame(frame).unwrap().unwrap();
        assert_eq!(
            event,
            Event::WorkspacesSnapshot(vec![WorkspaceState {
                name: "1".into(),
                output: Some("HDMI-1".into()),
                focused: true,
            }])
        );
    }

    #[test]
    fn decodes_focus_window_xid() {
        let frame = Frame {
            message_type: EVENT_WINDOW,
            payload: br#"{"change":"focus","container":{"window":4242}}"#.to_vec(),
        };
        assert_eq!(
            decode_frame(frame).unwrap(),
            Some(Event::WindowFocusedWithApp {
                window: Some(WindowId(4242)),
                app_name: None,
            })
        );
    }

    #[test]
    fn decodes_window_class_on_focus() {
        let frame = Frame {
            message_type: EVENT_WINDOW,
            payload: br#"{"change":"focus","container":{"window":4242,"window_properties":{"class":"Navigator","instance":"zen"}}}"#
                .to_vec(),
        };
        assert_eq!(
            decode_frame(frame).unwrap(),
            Some(Event::WindowFocusedWithApp {
                window: Some(WindowId(4242)),
                app_name: Some("Zen Browser".into()),
            })
        );
    }

    #[test]
    fn decodes_initial_focused_window_from_tree() {
        let frame = Frame {
            message_type: GET_TREE,
            payload: br#"{"focused":true,"window":null,"nodes":[{"focused":true,"window":42,"nodes":[],"floating_nodes":[]}] ,"floating_nodes":[]}"#.to_vec(),
        };
        assert_eq!(
            decode_frame(frame).unwrap(),
            Some(Event::WindowFocusedWithApp {
                window: Some(WindowId(42)),
                app_name: None,
            })
        );
    }

    #[test]
    fn initial_tree_without_window_is_valid() {
        let frame = Frame {
            message_type: GET_TREE,
            payload: br#"{"focused":true,"window":null,"nodes":[],"floating_nodes":[]}"#.to_vec(),
        };
        assert_eq!(
            decode_frame(frame).unwrap(),
            Some(Event::WindowFocusedWithApp {
                window: None,
                app_name: None,
            })
        );
    }

    #[test]
    fn ignores_non_focus_window_events_and_subscribe_ack() {
        let window = Frame {
            message_type: EVENT_WINDOW,
            payload: br#"{"change":"title","container":{"window":4242}}"#.to_vec(),
        };
        let ack = Frame {
            message_type: protocol::SUBSCRIBE,
            payload: br#"{"success":true}"#.to_vec(),
        };
        assert_eq!(decode_frame(window).unwrap(), None);
        assert_eq!(decode_frame(ack).unwrap(), None);
    }

    #[test]
    fn decodes_workspace_and_output_events() {
        let workspace = Frame {
            message_type: EVENT_WORKSPACE,
            payload: br#"{"change":"focus","current":{"name":"2"}}"#.to_vec(),
        };
        let output = Frame {
            message_type: EVENT_OUTPUT,
            payload: br#"{"change":"unspecified","output":"HDMI-1"}"#.to_vec(),
        };
        assert_eq!(
            decode_frame(workspace).unwrap(),
            Some(Event::WorkspaceFocused {
                name: Some("2".into())
            })
        );
        assert_eq!(
            decode_frame(output).unwrap(),
            Some(Event::X11(crate::platform::x11::X11Event::RandrChanged))
        );
    }
}
