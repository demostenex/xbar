use serde::Serialize;
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use std::time::Duration;
use zbus::zvariant::{OwnedValue, StructureBuilder, Type, Value};

const REGISTRAR_NAME: &str = "com.canonical.AppMenu.Registrar";
const REGISTRAR_PATH: &str = "/com/canonical/AppMenu/Registrar";

const TEST_XID: u32 = 0x02e0_0003;
const TEST_OBJECT_PATH: &str = "/com/xbar/FixtureMenu";

#[derive(Debug, Serialize, Type)]
struct FixtureNode(i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

struct Fixture {
    revision: Arc<AtomicU32>,
    prepared: Arc<std::sync::atomic::AtomicBool>,
    disabled_new: Arc<std::sync::atomic::AtomicBool>,
    hidden_new: Arc<std::sync::atomic::AtomicBool>,
    extra_export: Arc<std::sync::atomic::AtomicBool>,
    remove_recent: Arc<std::sync::atomic::AtomicBool>,
    prefix: String,
}

fn property(name: &str, value: OwnedValue) -> (String, OwnedValue) {
    (name.into(), value)
}
fn value<T>(value: T) -> OwnedValue
where
    T: Into<Value<'static>>,
{
    OwnedValue::try_from(value.into()).expect("fixture value")
}
fn item(id: i32, props: Vec<(String, OwnedValue)>, children: Vec<OwnedValue>) -> OwnedValue {
    OwnedValue::try_from(
        StructureBuilder::new()
            .add_field(id)
            .add_field(props.into_iter().collect::<HashMap<_, _>>())
            .add_field(children)
            .build()
            .expect("fixture layout"),
    )
    .expect("fixture item")
}

fn layout(
    prepared: bool,
    disabled_new: bool,
    hidden_new: bool,
    extra_export: bool,
    remove_recent: bool,
    prefix: &str,
) -> FixtureNode {
    let new = item(
        2,
        vec![
            property("label", value("Novo")),
            property("enabled", value(!disabled_new)),
            property("visible", value(!hidden_new)),
            property("shortcut", value(vec![vec!["<Control>", "N"]])),
        ],
        vec![],
    );
    let recent_a = item(7, vec![property("label", value("projeto-a"))], vec![]);
    let recent_b = item(8, vec![property("label", value("projeto-b"))], vec![]);
    let recent = item(
        9,
        vec![
            property("label", value("Recentes")),
            property("children-display", value("submenu")),
        ],
        if prepared && !remove_recent {
            vec![recent_a, recent_b]
        } else {
            vec![]
        },
    );
    let separator = item(10, vec![property("type", value("separator"))], vec![]);
    let quit = item(
        3,
        vec![
            property("label", value("Sair")),
            property("enabled", value(false)),
            property("icon-name", value("application-exit")),
        ],
        vec![],
    );
    let file = item(
        1,
        vec![
            property("label", value(format!("{}-_Arquivo", prefix))),
            property("children-display", value("submenu")),
        ],
        {
            let mut children = vec![new, recent, separator, quit];
            if extra_export {
                children.insert(
                    1,
                    item(13, vec![property("label", value("Exportar"))], vec![]),
                );
            }
            children
        },
    );
    let copy = item(11, vec![property("label", value("Copiar"))], vec![]);
    let paste = item(
        12,
        vec![
            property("label", value("Colar")),
            property("enabled", value(false)),
        ],
        vec![],
    );
    let edit = item(
        4,
        vec![
            property("label", value(format!("{}-Editar", prefix))),
            property("children-display", value("submenu")),
        ],
        vec![copy, paste],
    );
    let view = item(
        5,
        vec![property("label", value(format!("{}-Exibir", prefix)))],
        vec![],
    );
    let help = item(
        6,
        vec![
            property("label", value("Ajuda")),
            property("visible", value(false)),
        ],
        vec![],
    );
    FixtureNode(0, HashMap::new(), vec![file, edit, view, help])
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Fixture {
    fn ping(&self) {}

    fn get_layout(
        &self,
        _parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> (u32, FixtureNode) {
        (
            self.revision.load(Ordering::Relaxed),
            layout(
                self.prepared.load(Ordering::Relaxed),
                self.disabled_new.load(Ordering::Relaxed),
                self.hidden_new.load(Ordering::Relaxed),
                self.extra_export.load(Ordering::Relaxed),
                self.remove_recent.load(Ordering::Relaxed),
                &self.prefix,
            ),
        )
    }

    fn about_to_show(&self, id: i32) -> bool {
        eprintln!("fixture AboutToShow id={id}");
        if id == 9 && !self.prepared.swap(true, Ordering::Relaxed) {
            self.revision.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    fn event(&self, id: i32, event_id: &str, _data: OwnedValue, timestamp: u32) {
        let label = match id {
            2 => "Novo",
            7 => "projeto-a",
            8 => "projeto-b",
            11 => "Copiar",
            3 => "Sair",
            _ => "unknown",
        };
        eprintln!("fixture Event id={id} label={label} event_id={event_id} timestamp={timestamp}");
    }

    #[zbus(signal)]
    async fn layout_updated(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        revision: u32,
        parent_id: i32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn items_properties_updated(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        updated: Vec<(i32, HashMap<String, OwnedValue>)>,
        removed: Vec<(i32, Vec<String>)>,
    ) -> zbus::Result<()>;
}

fn main() -> Result<(), Box<dyn Error>> {
    zbus::block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    let unregister = args.iter().any(|arg| arg == "--unregister");
    let query = args.iter().any(|arg| arg == "--query");
    let update = args.iter().any(|arg| arg == "--update");
    let disable_new = args.iter().any(|arg| arg == "--disable-new");
    let hide_new = args.iter().any(|arg| arg == "--hide-new");
    let add_export = args.iter().any(|arg| arg == "--add-export");
    let remove_recent = args.iter().any(|arg| arg == "--remove-recent");
    let rename_new = args.iter().any(|arg| arg == "--rename-new");
    let prefix = args
        .iter()
        .position(|arg| arg == "--prefix")
        .and_then(|index| args.get(index + 1))
        .cloned()
        .unwrap_or_else(|| "Fixture".into());
    let xid = args
        .iter()
        .skip_while(|arg| *arg != "--xid")
        .nth(1)
        .map(|value| {
            if let Some(value) = value.strip_prefix("0x") {
                u32::from_str_radix(value, 16)
            } else {
                value.parse()
            }
        })
        .transpose()?
        .unwrap_or(TEST_XID);
    let revision = Arc::new(AtomicU32::new(1));
    let disabled_new = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hidden_new = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let extra_export = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let remove_recent_state = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let connection = zbus::connection::Builder::session()?
        .serve_at(
            TEST_OBJECT_PATH,
            Fixture {
                revision: revision.clone(),
                prepared: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                disabled_new: disabled_new.clone(),
                hidden_new: hidden_new.clone(),
                extra_export: extra_export.clone(),
                remove_recent: remove_recent_state.clone(),
                prefix,
            },
        )?
        .build()
        .await?;
    let proxy =
        zbus::Proxy::new(&connection, REGISTRAR_NAME, REGISTRAR_PATH, REGISTRAR_NAME).await?;
    let path: zbus::zvariant::ObjectPath<'_> = TEST_OBJECT_PATH.try_into()?;
    proxy.call_method("RegisterWindow", &(xid, path)).await?;
    println!(
        "registered xid={xid} path={TEST_OBJECT_PATH} sender={:?}",
        connection.unique_name()
    );
    if query {
        let (service, menu_path): (String, zbus::zvariant::OwnedObjectPath) =
            proxy.call("GetMenuForWindow", &xid).await?;
        println!("lookup xid={xid} service={service} path={menu_path}");
    }
    if update || disable_new || hide_new || add_export || remove_recent || rename_new {
        // Give xbar time to finish the first GetLayout and install its watcher.
        std::thread::sleep(Duration::from_millis(4000));
        disabled_new.store(disable_new, Ordering::Relaxed);
        hidden_new.store(hide_new, Ordering::Relaxed);
        extra_export.store(add_export, Ordering::Relaxed);
        remove_recent_state.store(remove_recent, Ordering::Relaxed);
        revision.store(2, Ordering::Relaxed);
        let interface = connection
            .object_server()
            .interface::<_, Fixture>(TEST_OBJECT_PATH)
            .await?;
        if disable_new || hide_new || rename_new {
            let mut updated = vec![(2, HashMap::new())];
            if disable_new {
                updated[0].1.insert("enabled".into(), value(false));
            }
            if hide_new {
                updated[0].1.insert("visible".into(), value(false));
            }
            if rename_new {
                updated[0]
                    .1
                    .insert("label".into(), value("Novo atualizado"));
            }
            Fixture::items_properties_updated(interface.signal_emitter(), updated, vec![]).await?;
            println!("emitted ItemsPropertiesUpdated for Novo");
        }
        if update || add_export || remove_recent {
            Fixture::layout_updated(interface.signal_emitter(), 2, 0).await?;
            println!("emitted LayoutUpdated revision=2 parent=0");
        }
    }
    if unregister {
        std::thread::sleep(Duration::from_millis(250));
        proxy.call_method("UnregisterWindow", &xid).await?;
        println!("unregistered xid={xid}");
        return Ok(());
    }
    std::thread::sleep(Duration::from_secs(30));
    Ok(())
}
