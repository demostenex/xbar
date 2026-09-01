use async_channel::Sender;
use std::collections::HashSet;
use std::io::{self, BufRead};
use std::sync::{Arc, Mutex};

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";

#[derive(Clone)]
struct Watcher {
    items: Arc<Mutex<Vec<String>>>,
    hosts: Arc<Mutex<HashSet<String>>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    async fn register_status_notifier_host(&self, host: String) -> zbus::fdo::Result<()> {
        self.hosts.lock().expect("hosts poisoned").insert(host);
        Ok(())
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().expect("items poisoned").clone()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        !self.hosts.lock().expect("hosts poisoned").is_empty()
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        item: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        item: String,
    ) -> zbus::Result<()>;
}

enum Command {
    Add(String),
    Remove(String),
    Quit,
}

fn command_reader(sender: Sender<Command>) {
    for line in io::stdin().lock().lines().map_while(Result::ok) {
        let mut parts = line.split_whitespace();
        let Some(command) = parts.next() else {
            continue;
        };
        let result = match command {
            "add" => parts.next().map(|item| Command::Add(item.into())),
            "remove" => parts.next().map(|item| Command::Remove(item.into())),
            "quit" => Some(Command::Quit),
            _ => None,
        };
        if let Some(command) = result {
            if sender.send_blocking(command).is_err() {
                break;
            }
        }
    }
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    let watcher = Watcher {
        items: Arc::new(Mutex::new(Vec::new())),
        hosts: Arc::new(Mutex::new(HashSet::new())),
    };
    let connection = zbus::connection::Builder::session()?
        .name(WATCHER_NAME)?
        .allow_name_replacements(false)
        .replace_existing_names(false)
        .serve_at(WATCHER_PATH, watcher.clone())?
        .build()
        .await?;
    let service = connection
        .unique_name()
        .ok_or("watcher fixture has no unique name")?
        .to_string();
    let initial = format!("{service}/StatusNotifierItem");
    watcher
        .items
        .lock()
        .expect("items poisoned")
        .push(initial.clone());
    let (sender, receiver) = async_channel::unbounded();
    std::thread::spawn(move || command_reader(sender));
    eprintln!("sni-watcher-fixture service={service} initial={initial}");

    while let Ok(command) = receiver.recv().await {
        match command {
            Command::Add(item) => {
                let added = {
                    let mut items = watcher.items.lock().expect("items poisoned");
                    if items.contains(&item) {
                        false
                    } else {
                        items.push(item.clone());
                        true
                    }
                };
                if added {
                    let emitter =
                        zbus::object_server::SignalEmitter::new(&connection, WATCHER_PATH)?;
                    Watcher::status_notifier_item_registered(&emitter, item).await?;
                }
            }
            Command::Remove(item) => {
                let removed = {
                    let mut items = watcher.items.lock().expect("items poisoned");
                    let Some(index) = items.iter().position(|value| value == &item) else {
                        continue;
                    };
                    items.remove(index);
                    true
                };
                if removed {
                    let emitter =
                        zbus::object_server::SignalEmitter::new(&connection, WATCHER_PATH)?;
                    Watcher::status_notifier_item_unregistered(&emitter, item).await?;
                }
            }
            Command::Quit => break,
        }
    }
    drop(connection);
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    zbus::block_on(async_main())
}
