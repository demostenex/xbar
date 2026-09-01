use serde::Serialize;
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::io::{self, BufRead};
use std::sync::{Arc, Mutex};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, StructureBuilder, Type, Value};

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_INTERFACE: &str = "org.kde.StatusNotifierWatcher";

struct FixtureItem {
    status: Arc<Mutex<String>>,
    item_is_menu: Arc<Mutex<bool>>,
    menu_path: Arc<Mutex<Option<String>>>,
    color: u8,
}

#[derive(Debug, Serialize, Type)]
struct MenuNode(i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

fn menu_value<T: Into<Value<'static>>>(value: T) -> OwnedValue {
    OwnedValue::try_from(value.into()).expect("menu fixture value")
}

fn menu_item(id: i32, label: &str) -> OwnedValue {
    OwnedValue::try_from(
        StructureBuilder::new()
            .add_field(id)
            .add_field(HashMap::from([(
                String::from("label"),
                menu_value(label.to_string()),
            )]))
            .add_field(Vec::<OwnedValue>::new())
            .build()
            .expect("menu fixture item"),
    )
    .expect("menu fixture owned item")
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl FixtureItem {
    #[zbus(property)]
    fn status(&self) -> String {
        self.status.lock().expect("status poisoned").clone()
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        *self.item_is_menu.lock().expect("item_is_menu poisoned")
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        self.menu_path
            .lock()
            .expect("menu path poisoned")
            .as_deref()
            .unwrap_or("/")
            .try_into()
            .expect("menu path")
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let mut pixels = Vec::with_capacity(16 * 16 * 4);
        for y in 0..16 {
            for x in 0..16 {
                let alpha = 255_u8;
                let red: u8 = if (x + y) % 2 == 0 { 0x30 } else { 0x80 };
                pixels.extend_from_slice(&[alpha, red.saturating_add(self.color), 0x90, 0xe0]);
            }
        }
        vec![(16, 16, pixels)]
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        self.icon_pixmap()
    }

    async fn activate(&self, x: i32, y: i32) -> zbus::fdo::Result<()> {
        eprintln!("sni-fixture action=Activate x={x} y={y}");
        Ok(())
    }

    async fn secondary_activate(&self, x: i32, y: i32) -> zbus::fdo::Result<()> {
        eprintln!("sni-fixture action=SecondaryActivate x={x} y={y}");
        Ok(())
    }

    async fn context_menu(&self, x: i32, y: i32) -> zbus::fdo::Result<()> {
        eprintln!("sni-fixture action=ContextMenu x={x} y={y}");
        Ok(())
    }

    async fn scroll(&self, delta: i32, orientation: String) -> zbus::fdo::Result<()> {
        eprintln!("sni-fixture action=Scroll delta={delta} orientation={orientation}");
        Ok(())
    }

    #[zbus(signal)]
    async fn new_status(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        status: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_icon(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_attention_icon(
        emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_item_is_menu(emitter: &zbus::object_server::SignalEmitter<'_>)
        -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_menu(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}

struct MenuFixture;

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl MenuFixture {
    fn get_layout(&self, _parent: i32, _depth: i32, _properties: Vec<String>) -> (u32, MenuNode) {
        (
            1,
            MenuNode(
                0,
                HashMap::new(),
                vec![
                    menu_item(1, "Abrir"),
                    menu_item(2, "Opção"),
                    menu_item(3, "Sair"),
                ],
            ),
        )
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    fn event(&self, id: i32, event_id: &str, _data: OwnedValue, timestamp: u32) {
        eprintln!("sni-fixture menu-event id={id} event={event_id} timestamp={timestamp}");
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    zbus::block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    let path = args
        .iter()
        .position(|arg| arg == "--path")
        .and_then(|index| args.get(index + 1))
        .cloned()
        .unwrap_or_else(|| "/StatusNotifierItem".into());
    let second = args.iter().any(|arg| arg == "--second");
    let unregister = args.iter().any(|arg| arg == "--unregister");
    let color = args
        .iter()
        .position(|arg| arg == "--color")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(0);
    let status = Arc::new(Mutex::new(
        if args.iter().any(|arg| arg == "--passive") {
            "Passive"
        } else if args.iter().any(|arg| arg == "--attention") {
            "NeedsAttention"
        } else {
            "Active"
        }
        .into(),
    ));
    let item_is_menu = Arc::new(Mutex::new(false));
    let menu_path = Arc::new(Mutex::new(Some("/StatusNotifierMenu".into())));
    let item_path = path.clone();
    let connection = zbus::connection::Builder::session()?
        .serve_at(
            item_path.as_str(),
            FixtureItem {
                status: status.clone(),
                item_is_menu: item_is_menu.clone(),
                menu_path: menu_path.clone(),
                color,
            },
        )?
        .serve_at("/StatusNotifierMenu", MenuFixture)?
        .serve_at("/MenuA", MenuFixture)?
        .serve_at("/MenuB", MenuFixture)?
        .build()
        .await?;
    let watcher = zbus::Proxy::new_owned(
        connection.clone(),
        WATCHER_NAME,
        WATCHER_PATH,
        WATCHER_INTERFACE,
    )
    .await?;
    let service = connection
        .unique_name()
        .ok_or("fixture connection has no unique name")?
        .to_string();
    let register = |path: String| {
        let watcher = watcher.clone();
        async move {
            let _: () = watcher.call("RegisterStatusNotifierItem", &(path,)).await?;
            Ok::<(), zbus::Error>(())
        }
    };
    register(path.clone()).await?;
    if second {
        register("/StatusNotifierItem/2".into()).await?;
    }
    if unregister {
        eprintln!("sni-fixture registered service={service} path={path}");
        return Ok(());
    }
    eprintln!("sni-fixture registered service={service} path={path}");
    let (sender, receiver) = async_channel::unbounded::<String>();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines().map_while(Result::ok) {
            if sender.send_blocking(line).is_err() {
                break;
            }
        }
    });
    while let Ok(command) = receiver.recv().await {
        let next = match command.trim() {
            "active" => Some("Active"),
            "passive" => Some("Passive"),
            "attention" => Some("NeedsAttention"),
            "quit" => break,
            _ => None,
        };
        if let Some(next) = next {
            *status.lock().expect("status poisoned") = next.into();
            let emitter = zbus::object_server::SignalEmitter::new(&connection, path.as_str())?;
            FixtureItem::new_status(&emitter, next.into()).await?;
        } else if matches!(command.trim(), "menu" | "normal") {
            let value = command.trim() == "menu";
            *item_is_menu.lock().expect("item_is_menu poisoned") = value;
            let emitter = zbus::object_server::SignalEmitter::new(&connection, path.as_str())?;
            FixtureItem::new_item_is_menu(&emitter).await?;
        } else if matches!(command.trim(), "menu-clear" | "menu-a" | "menu-b") {
            let next = match command.trim() {
                "menu-clear" => None,
                "menu-a" => Some("/MenuA"),
                "menu-b" => Some("/MenuB"),
                _ => unreachable!(),
            };
            *menu_path.lock().expect("menu path poisoned") = next.map(str::to_owned);
            let emitter = zbus::object_server::SignalEmitter::new(&connection, path.as_str())?;
            FixtureItem::new_menu(&emitter).await?;
        }
    }
    Ok(())
}
