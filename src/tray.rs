//! System tray — StatusNotifierItem host over D-Bus.
//!
//! A dedicated thread runs an async zbus connection (`zbus::block_on`);
//! per-item signal watchers run as tasks on the connection's internal
//! executor. State updates flow to the main calloop as full snapshots
//! ([`TrayEvent::Items`]) over a calloop channel — same direction and
//! debounce-free shape as the workspace/toplevel protocols. Commands
//! (activate, menu fetch, menu clicks) flow back over an async-channel
//! and execute on the tray thread, so a hung tray app can stall at most
//! the command queue, never the bar's render loop.
//!
//! Watcher arbitration: if `org.kde.StatusNotifierWatcher` is unowned
//! (the normal case under prism) we claim it and serve the watcher
//! interface ourselves; if a watcher already exists we register as a
//! host with it and mirror its item list.
//!
//! Menus are `com.canonical.dbusmenu`: fetched on demand with
//! `AboutToShow` + `GetLayout` ([`TrayCmd::MenuOpen`] →
//! [`TrayEvent::Menu`]), clicks reported with `Event(id, "clicked")`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use zbus::fdo::{DBusProxy, RequestNameFlags, RequestNameReply};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Structure, Value};
use zbus::Connection;

use damascene_core::image::Image;
use damascene_core::SvgIcon;

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// `bus name` + `object path` identifying one StatusNotifierItem.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Address {
    bus: String,
    path: String,
}

impl Address {
    /// Parse a watcher registration string. The wire formats in the
    /// wild: a bare bus name (path defaults to `/StatusNotifierItem`),
    /// `bus/path` concatenated (KDE's watcher stores them this way),
    /// or a bare path (libappindicator registers the path only; the
    /// bus is the message sender).
    fn parse(service: &str, sender: Option<&str>) -> Option<Self> {
        if service.starts_with('/') {
            return Some(Self {
                bus: sender?.to_string(),
                path: service.to_string(),
            });
        }
        match service.find('/') {
            Some(i) => Some(Self {
                bus: service[..i].to_string(),
                path: service[i..].to_string(),
            }),
            None => Some(Self {
                bus: service.to_string(),
                path: "/StatusNotifierItem".to_string(),
            }),
        }
    }

    /// Stable map/sort key.
    fn key(&self) -> String {
        format!("{}{}", self.bus, self.path)
    }
}

/// Resolved item icon, ready for the damascene tree.
#[derive(Clone)]
pub enum TrayIcon {
    Raster(Image),
    Svg(SvgIcon),
}

/// One tray item as the bar renders it.
#[derive(Clone)]
pub struct TrayItem {
    pub address: Address,
    /// Tooltip-ish display name (`Title` property).
    pub title: String,
    pub icon: Option<TrayIcon>,
    /// Item asks left-click to open the menu instead of `Activate`.
    pub item_is_menu: bool,
    /// A dbusmenu path exists, so a context menu can be requested.
    pub has_menu: bool,
}

/// One parsed dbusmenu node. `children` non-empty means submenu.
#[derive(Clone, Debug, Default)]
pub struct MenuNode {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub separator: bool,
    /// Checkmark/radio state when the item is a toggle.
    pub toggle: Option<bool>,
    pub children: Vec<MenuNode>,
}

/// Tray thread → main loop.
pub enum TrayEvent {
    /// Full snapshot of visible (non-Passive) items, sorted by address.
    Items(Vec<TrayItem>),
    /// Menu layout reply; `root.children` are the top-level entries.
    Menu { address: Address, root: MenuNode },
    /// Menu fetch failed; the bar should not open a popup.
    MenuError { address: Address },
}

/// Main loop → tray thread.
pub enum TrayCmd {
    /// Primary activation (left click). Coordinates are screen-relative
    /// per spec; we don't know our global position, so send zeros —
    /// items overwhelmingly ignore them.
    Activate(Address),
    /// Middle click.
    SecondaryActivate(Address),
    /// Fetch the menu layout (`AboutToShow` + `GetLayout`).
    MenuOpen(Address),
    /// A menu entry was clicked.
    MenuClicked { address: Address, id: i32 },
    /// The menu popup was dismissed (dbusmenu "closed" event, root id).
    MenuClosed(Address),
}

/// Main-thread handle: owns the command channel. Dropping it closes
/// the channel, which ends the tray thread's command loop and with it
/// the connection (releasing the watcher name).
pub struct Tray {
    cmd_tx: async_channel::Sender<TrayCmd>,
}

impl Tray {
    /// Spawn the tray thread. Failures to reach the bus are logged and
    /// leave the tray empty — the bar runs without it.
    pub fn spawn(events: Sender<TrayEvent>) -> Self {
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        if let Err(err) = std::thread::Builder::new()
            .name("tray-dbus".into())
            .spawn(move || {
                if let Err(err) = zbus::block_on(run(events, cmd_rx)) {
                    tracing::error!("tray thread exited: {err:#}");
                }
            })
        {
            tracing::error!(%err, "failed to spawn tray thread");
        }
        Self { cmd_tx }
    }

    pub fn send(&self, cmd: TrayCmd) {
        // Unbounded channel: send_blocking never actually blocks.
        if self.cmd_tx.send_blocking(cmd).is_err() {
            tracing::warn!("tray thread gone; dropping command");
        }
    }
}

// ---------------------------------------------------------------------------
// D-Bus interfaces
// ---------------------------------------------------------------------------

#[zbus::proxy(
    interface = "org.kde.StatusNotifierItem",
    assume_defaults = false,
    gen_blocking = false
)]
trait StatusNotifierItem {
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;

    // Items signal property changes with bespoke NewIcon/NewStatus
    // signals, never PropertiesChanged — disable zbus's cache so every
    // read is live.
    #[zbus(property(emits_changed_signal = "false"))]
    fn title(&self) -> zbus::Result<String>;
    #[zbus(property(emits_changed_signal = "false"))]
    fn status(&self) -> zbus::Result<String>;
    #[zbus(property(emits_changed_signal = "false"))]
    fn icon_name(&self) -> zbus::Result<String>;
    #[zbus(property(emits_changed_signal = "false"))]
    fn icon_pixmap(&self) -> zbus::Result<Vec<(i32, i32, Vec<u8>)>>;
    #[zbus(property(emits_changed_signal = "false"))]
    fn attention_icon_name(&self) -> zbus::Result<String>;
    #[zbus(property(emits_changed_signal = "false"))]
    fn attention_icon_pixmap(&self) -> zbus::Result<Vec<(i32, i32, Vec<u8>)>>;
    #[zbus(property(emits_changed_signal = "false"))]
    fn icon_theme_path(&self) -> zbus::Result<String>;
    #[zbus(property(emits_changed_signal = "false"))]
    fn menu(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property(emits_changed_signal = "false"))]
    fn item_is_menu(&self) -> zbus::Result<bool>;
}

/// Raw `GetLayout` node: `(ia{sv}av)`.
type RawMenuNode = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

#[zbus::proxy(
    interface = "com.canonical.dbusmenu",
    assume_defaults = false,
    gen_blocking = false
)]
trait DBusMenu {
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<&str>,
    ) -> zbus::Result<(u32, RawMenuNode)>;
    fn event(&self, id: i32, event_id: &str, data: &Value<'_>, timestamp: u32) -> zbus::Result<()>;
    fn about_to_show(&self, id: i32) -> zbus::Result<bool>;
}

#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher",
    gen_blocking = false
)]
trait StatusNotifierWatcher {
    fn register_status_notifier_host(&self, service: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> zbus::Result<Vec<String>>;
    #[zbus(signal)]
    fn status_notifier_item_registered(&self, service: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn status_notifier_item_unregistered(&self, service: String) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// Shared tray state
// ---------------------------------------------------------------------------

struct ItemState {
    address: Address,
    /// The raw string the item registered with (echoed in watcher
    /// signals and the RegisteredStatusNotifierItems property).
    registered: String,
    view: TrayItem,
    /// Spec: Passive items should be hidden.
    passive: bool,
    menu_path: Option<OwnedObjectPath>,
    /// Signal-watch task; dropping it (on removal) cancels the watch.
    _task: zbus::Task<()>,
}

struct State {
    /// Keyed by `Address::key()`; BTreeMap iteration gives the
    /// stable presentation order.
    items: BTreeMap<String, ItemState>,
    events: Sender<TrayEvent>,
}

type Shared = Arc<Mutex<State>>;

fn push_snapshot(state: &Shared) {
    let guard = state.lock().unwrap();
    let items: Vec<TrayItem> = guard
        .items
        .values()
        .filter(|i| !i.passive)
        .map(|i| i.view.clone())
        .collect();
    if guard.events.send(TrayEvent::Items(items)).is_err() {
        tracing::warn!("main loop channel closed; dropping tray snapshot");
    }
}

// ---------------------------------------------------------------------------
// Watcher interface (served when we own the name)
// ---------------------------------------------------------------------------

struct Watcher {
    state: Shared,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    async fn register_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let sender = header.sender().map(|s| s.to_string());
        let Some(address) = Address::parse(service, sender.as_deref()) else {
            tracing::warn!(service, "unparseable item registration; ignoring");
            return;
        };
        tracing::debug!(service, bus = %address.bus, path = %address.path, "item registered");
        let _ = Self::status_notifier_item_registered(&emitter, service.to_string()).await;
        // Property fetches round-trip to the registering app; run them
        // as a task so a slow app doesn't stall the object server.
        conn.executor()
            .spawn(
                add_item(
                    self.state.clone(),
                    conn.clone(),
                    address,
                    service.to_string(),
                ),
                "tray-add-item",
            )
            .detach();
    }

    fn register_status_notifier_host(&self, _service: &str) {
        // We are a host ourselves and don't fan items out to others;
        // accepting the registration is enough for well-behaved items.
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .items
            .values()
            .map(|i| i.registered.clone())
            .collect()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &SignalEmitter<'_>,
        service: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &SignalEmitter<'_>,
        service: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// Item lifecycle
// ---------------------------------------------------------------------------

async fn item_proxy(
    conn: &Connection,
    address: &Address,
) -> Result<StatusNotifierItemProxy<'static>> {
    StatusNotifierItemProxy::builder(conn)
        .destination(address.bus.clone())?
        .path(address.path.clone())?
        .build()
        .await
        .context("item proxy")
}

async fn add_item(state: Shared, conn: Connection, address: Address, registered: String) {
    let key = address.key();
    if state.lock().unwrap().items.contains_key(&key) {
        return;
    }
    let proxy = match item_proxy(&conn, &address).await {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(item = %key, %err, "item proxy failed");
            return;
        }
    };
    let (view, passive) = fetch_view(&proxy, &address).await;
    let menu_path = proxy.menu().await.ok().filter(|p| p.as_str() != "/");
    let task = conn.executor().spawn(
        watch_item(state.clone(), proxy.clone(), address.clone()),
        "tray-item-signals",
    );
    state.lock().unwrap().items.insert(
        key,
        ItemState {
            address,
            registered,
            view,
            passive,
            menu_path,
            _task: task,
        },
    );
    push_snapshot(&state);
}

fn remove_item(state: &Shared, key: &str) -> Option<String> {
    let registered = state
        .lock()
        .unwrap()
        .items
        .remove(key)
        .map(|i| i.registered);
    if registered.is_some() {
        tracing::debug!(item = %key, "item removed");
        push_snapshot(state);
    }
    registered
}

/// Re-fetch everything the snapshot needs. Properties are best-effort:
/// items routinely implement only a subset, so each falls back to a
/// default rather than failing the item.
async fn fetch_view(proxy: &StatusNotifierItemProxy<'_>, address: &Address) -> (TrayItem, bool) {
    let status = proxy.status().await.unwrap_or_default();
    let passive = status == "Passive";
    let attention = status == "NeedsAttention";
    let title = proxy.title().await.unwrap_or_default();
    let theme_path = proxy.icon_theme_path().await.unwrap_or_default();
    let item_is_menu = proxy.item_is_menu().await.unwrap_or(false);
    let has_menu = matches!(proxy.menu().await, Ok(ref p) if p.as_str() != "/");

    // Attention status prefers the attention icon, falling back to the
    // regular one. Within each: theme lookup by name first (crisper,
    // theme-consistent), pixmap property second (Electron-style apps
    // ship only the pixmap).
    let mut icon = None;
    if attention {
        if let Ok(name) = proxy.attention_icon_name().await {
            icon = lookup_icon(&name, &theme_path);
        }
        if icon.is_none() {
            if let Ok(pixmaps) = proxy.attention_icon_pixmap().await {
                icon = best_pixmap(pixmaps);
            }
        }
    }
    if icon.is_none() {
        if let Ok(name) = proxy.icon_name().await {
            icon = lookup_icon(&name, &theme_path);
        }
    }
    if icon.is_none() {
        if let Ok(pixmaps) = proxy.icon_pixmap().await {
            icon = best_pixmap(pixmaps);
        }
    }
    if icon.is_none() {
        tracing::debug!(item = %address.key(), "no resolvable icon");
    }

    (
        TrayItem {
            address: address.clone(),
            title,
            icon,
            item_is_menu,
            has_menu,
        },
        passive,
    )
}

/// React to the item's change signals by re-fetching the full view —
/// the signals carry no payload, and a full refresh keeps this simple.
async fn watch_item(state: Shared, proxy: StatusNotifierItemProxy<'static>, address: Address) {
    let mut signals = match proxy.inner().receive_all_signals().await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(item = %address.key(), %err, "signal subscription failed");
            return;
        }
    };
    while let Some(msg) = signals.next().await {
        let header = msg.header();
        let Some(member) = header.member() else {
            continue;
        };
        match member.as_str() {
            "NewIcon" | "NewAttentionIcon" | "NewStatus" | "NewTitle" | "NewToolTip" => {
                let (view, passive) = fetch_view(&proxy, &address).await;
                {
                    let mut guard = state.lock().unwrap();
                    let Some(item) = guard.items.get_mut(&address.key()) else {
                        return; // removed while we were fetching
                    };
                    item.view = view;
                    item.passive = passive;
                }
                push_snapshot(&state);
            }
            _ => {}
        }
    }
}

/// Owned-watcher mode: cull items whose bus connection died, emitting
/// the watcher's Unregistered signal for other hosts.
async fn watch_name_owners(state: Shared, conn: Connection) {
    let stream = async {
        DBusProxy::new(&conn)
            .await?
            .receive_name_owner_changed()
            .await
    };
    let mut stream = match stream.await {
        Ok(s) => s,
        Err(err) => {
            tracing::error!(%err, "NameOwnerChanged subscription failed; dead items will linger");
            return;
        }
    };
    while let Some(signal) = stream.next().await {
        let Ok(args) = signal.args() else { continue };
        if args.new_owner.is_none() {
            let name = args.name.as_str();
            let keys: Vec<String> = {
                let guard = state.lock().unwrap();
                guard
                    .items
                    .values()
                    .filter(|i| i.address.bus == name)
                    .map(|i| i.address.key())
                    .collect()
            };
            for key in keys {
                if let Some(registered) = remove_item(&state, &key) {
                    if let Ok(iface) = conn
                        .object_server()
                        .interface::<_, Watcher>(WATCHER_PATH)
                        .await
                    {
                        let _ = Watcher::status_notifier_item_unregistered(
                            iface.signal_emitter(),
                            registered,
                        )
                        .await;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Thread main
// ---------------------------------------------------------------------------

async fn run(events: Sender<TrayEvent>, cmd_rx: async_channel::Receiver<TrayCmd>) -> Result<()> {
    let conn = Connection::session().await.context("session bus")?;
    let state: Shared = Arc::new(Mutex::new(State {
        items: BTreeMap::new(),
        events: events.clone(),
    }));

    // Keep-alive handles for the mode-specific watcher tasks.
    let mut _tasks: Vec<zbus::Task<()>> = Vec::new();

    // Serve the watcher interface before requesting the name, so items
    // racing the registration never see the name without the object.
    conn.object_server()
        .at(
            WATCHER_PATH,
            Watcher {
                state: state.clone(),
            },
        )
        .await
        .context("serve watcher interface")?;

    let reply = DBusProxy::new(&conn)
        .await?
        .request_name(
            WATCHER_NAME.try_into().expect("static name"),
            RequestNameFlags::DoNotQueue.into(),
        )
        .await
        .context("request watcher name")?;

    if reply == RequestNameReply::PrimaryOwner {
        tracing::info!("acting as {WATCHER_NAME}");
        _tasks.push(conn.executor().spawn(
            watch_name_owners(state.clone(), conn.clone()),
            "tray-name-owners",
        ));
    } else {
        tracing::info!("existing {WATCHER_NAME}; registering as host");
        conn.object_server()
            .remove::<Watcher, _>(WATCHER_PATH)
            .await?;
        let watcher = StatusNotifierWatcherProxy::new(&conn)
            .await
            .context("watcher proxy")?;
        let unique = conn
            .unique_name()
            .map(|n| n.to_string())
            .unwrap_or_default();
        if let Err(err) = watcher.register_status_notifier_host(&unique).await {
            tracing::warn!(%err, "host registration failed; continuing");
        }

        for service in watcher
            .registered_status_notifier_items()
            .await
            .unwrap_or_default()
        {
            if let Some(address) = Address::parse(&service, None) {
                conn.executor()
                    .spawn(
                        add_item(state.clone(), conn.clone(), address, service),
                        "tray-add-item",
                    )
                    .detach();
            }
        }

        let mut registered = watcher
            .receive_status_notifier_item_registered()
            .await
            .context("subscribe item-registered")?;
        let mut unregistered = watcher
            .receive_status_notifier_item_unregistered()
            .await
            .context("subscribe item-unregistered")?;
        {
            let (state, conn) = (state.clone(), conn.clone());
            _tasks.push(conn.clone().executor().spawn(
                async move {
                    while let Some(signal) = registered.next().await {
                        let Ok(args) = signal.args() else { continue };
                        if let Some(address) = Address::parse(&args.service, None) {
                            add_item(state.clone(), conn.clone(), address, args.service.clone())
                                .await;
                        }
                    }
                },
                "tray-ext-registered",
            ));
        }
        {
            let state = state.clone();
            _tasks.push(conn.executor().spawn(
                async move {
                    while let Some(signal) = unregistered.next().await {
                        let Ok(args) = signal.args() else { continue };
                        if let Some(address) = Address::parse(&args.service, None) {
                            remove_item(&state, &address.key());
                        }
                    }
                },
                "tray-ext-unregistered",
            ));
        }
    }

    // Command loop; ends when the main thread drops its `Tray`.
    while let Ok(cmd) = cmd_rx.recv().await {
        handle_cmd(cmd, &state, &conn, &events).await;
    }
    Ok(())
}

async fn handle_cmd(cmd: TrayCmd, state: &Shared, conn: &Connection, events: &Sender<TrayEvent>) {
    match cmd {
        TrayCmd::Activate(address) => {
            if let Ok(proxy) = item_proxy(conn, &address).await {
                if let Err(err) = proxy.activate(0, 0).await {
                    tracing::debug!(item = %address.key(), %err, "Activate failed");
                }
            }
        }
        TrayCmd::SecondaryActivate(address) => {
            if let Ok(proxy) = item_proxy(conn, &address).await {
                if let Err(err) = proxy.secondary_activate(0, 0).await {
                    tracing::debug!(item = %address.key(), %err, "SecondaryActivate failed");
                }
            }
        }
        TrayCmd::MenuOpen(address) => {
            let root = fetch_menu(state, conn, &address).await;
            let event = match root {
                Ok(root) => TrayEvent::Menu { address, root },
                Err(err) => {
                    tracing::warn!(item = %address.key(), err = %format!("{err:#}"), "menu fetch failed");
                    TrayEvent::MenuError { address }
                }
            };
            let _ = events.send(event);
        }
        TrayCmd::MenuClicked { address, id } => {
            if let Ok(proxy) = menu_proxy(state, conn, &address).await {
                let _ = proxy.about_to_show(id).await;
                if let Err(err) = proxy.event(id, "clicked", &Value::I32(0), unix_now()).await {
                    tracing::debug!(item = %address.key(), %err, "menu click failed");
                }
            }
        }
        TrayCmd::MenuClosed(address) => {
            if let Ok(proxy) = menu_proxy(state, conn, &address).await {
                let _ = proxy.event(0, "closed", &Value::I32(0), unix_now()).await;
            }
        }
    }
}

fn unix_now() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// dbusmenu
// ---------------------------------------------------------------------------

async fn menu_proxy(
    state: &Shared,
    conn: &Connection,
    address: &Address,
) -> Result<DBusMenuProxy<'static>> {
    let path = state
        .lock()
        .unwrap()
        .items
        .get(&address.key())
        .and_then(|i| i.menu_path.clone())
        .context("item has no menu")?;
    DBusMenuProxy::builder(conn)
        .destination(address.bus.clone())?
        .path(path)?
        .build()
        .await
        .context("menu proxy")
}

async fn fetch_menu(state: &Shared, conn: &Connection, address: &Address) -> Result<MenuNode> {
    let proxy = menu_proxy(state, conn, address).await?;
    // Spec-required nicety; apps use it to (re)build dynamic menus.
    let _ = proxy.about_to_show(0).await;
    let (_, raw) = proxy
        .get_layout(
            0,
            -1,
            vec![
                "label",
                "enabled",
                "visible",
                "type",
                "children-display",
                "toggle-type",
                "toggle-state",
            ],
        )
        .await
        .context("GetLayout")?;
    let (id, props, children) = raw;
    menu_node(id, &props, &children).context("root node invisible")
}

fn menu_node(
    id: i32,
    props: &HashMap<String, OwnedValue>,
    children: &[OwnedValue],
) -> Option<MenuNode> {
    let get_bool = |key: &str, default: bool| -> bool {
        props
            .get(key)
            .and_then(|v| v.downcast_ref::<bool>().ok())
            .unwrap_or(default)
    };
    let get_str = |key: &str| -> Option<&str> { props.get(key)?.downcast_ref::<&str>().ok() };

    if !get_bool("visible", true) {
        return None;
    }
    if get_str("type") == Some("separator") {
        return Some(MenuNode {
            id,
            separator: true,
            ..Default::default()
        });
    }
    let toggle = match (
        get_str("toggle-type").unwrap_or(""),
        props
            .get("toggle-state")
            .and_then(|v| v.downcast_ref::<i32>().ok())
            .unwrap_or(-1),
    ) {
        ("", _) | (_, -1) => None,
        (_, state) => Some(state == 1),
    };
    Some(MenuNode {
        id,
        label: strip_access_key(get_str("label").unwrap_or("")),
        enabled: get_bool("enabled", true),
        separator: false,
        toggle,
        children: children.iter().filter_map(child_node).collect(),
    })
}

/// Parse one `av` element: a variant wrapping an `(ia{sv}av)` structure.
fn child_node(value: &OwnedValue) -> Option<MenuNode> {
    let value: &Value = value;
    let value = match value {
        Value::Value(inner) => inner,
        other => other,
    };
    let s: &Structure = value.downcast_ref().ok()?;
    let f = s.fields();
    if f.len() != 3 {
        return None;
    }
    let id: i32 = f[0].downcast_ref().ok()?;
    let props: HashMap<String, OwnedValue> = f[1].try_clone().ok()?.try_into().ok()?;
    let children: Vec<OwnedValue> = f[2].try_clone().ok()?.try_into().ok()?;
    menu_node(id, &props, &children)
}

/// dbusmenu labels mark access keys with `_` (and escape a literal
/// underscore as `__`); strip the markers for display.
fn strip_access_key(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars();
    while let Some(c) = chars.next() {
        if c == '_' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Icon resolution
// ---------------------------------------------------------------------------

/// Render target is ~22 logical px at up to 2× scale.
const ICON_TARGET: u32 = 44;

/// Decode the best-sized entry of an `a(iiay)` pixmap list (ARGB32,
/// network byte order, per spec).
fn best_pixmap(pixmaps: Vec<(i32, i32, Vec<u8>)>) -> Option<TrayIcon> {
    let (w, h, mut data) = pixmaps
        .into_iter()
        .filter(|(w, h, data)| *w > 0 && *h > 0 && data.len() == (*w as usize) * (*h as usize) * 4)
        // Smallest dimension ≥ target, else the largest available.
        .min_by_key(|(w, _, _)| {
            let w = *w as u32;
            if w >= ICON_TARGET {
                (0, w)
            } else {
                (1, u32::MAX - w)
            }
        })?;
    for px in data.chunks_exact_mut(4) {
        px.rotate_left(1); // [A,R,G,B] (big-endian u32) → [R,G,B,A]
    }
    Some(TrayIcon::Raster(Image::from_rgba8(
        w as u32, h as u32, data,
    )))
}

/// Freedesktop-ish icon lookup, deliberately partial: the app's own
/// `IconThemePath` first, then hicolor in the XDG data dirs, then flat
/// pixmaps. No theme inheritance — tray apps install into hicolor (the
/// mandated fallback) almost without exception, and chasing the user's
/// GTK theme isn't worth the complexity here.
fn lookup_icon(name: &str, theme_path: &str) -> Option<TrayIcon> {
    if name.is_empty() {
        return None;
    }
    // Some apps pass an absolute file path as the "name".
    if name.starts_with('/') {
        return load_icon_file(Path::new(name));
    }

    let mut best: Option<(u32, PathBuf)> = None;

    if !theme_path.is_empty() {
        let tp = Path::new(theme_path);
        // Both layouts exist in the wild: icons directly in the dir,
        // or a theme tree (sized subdirectories) rooted there.
        for ext in ["svg", "png"] {
            let direct = tp.join(format!("{name}.{ext}"));
            if direct.is_file() {
                consider(&mut best, if ext == "svg" { SCALABLE } else { 0 }, direct);
            }
        }
        search_theme_root(tp, name, &mut best);
        search_theme_root(&tp.join("hicolor"), name, &mut best);
    }

    for dir in xdg_data_dirs() {
        search_theme_root(&dir.join("icons/hicolor"), name, &mut best);
    }
    if best.is_none() {
        for dir in xdg_data_dirs() {
            for ext in ["svg", "png"] {
                let p = dir.join(format!("pixmaps/{name}.{ext}"));
                if p.is_file() {
                    consider(&mut best, if ext == "svg" { SCALABLE } else { 0 }, p);
                }
            }
        }
    }

    let (_, path) = best?;
    load_icon_file(&path)
}

/// Keep the better-ranked candidate (see [`icon_size_rank`]).
fn consider(best: &mut Option<(u32, PathBuf)>, size: u32, path: PathBuf) {
    let better = match best {
        None => true,
        Some((cur, _)) => icon_size_rank(size) < icon_size_rank(*cur),
    };
    if better {
        *best = Some((size, path));
    }
}

/// Pseudo-size marking scalable (svg) entries — always preferred.
const SCALABLE: u32 = u32::MAX;

/// Lower rank is better: svg, then smallest raster ≥ target, then
/// largest raster below it.
fn icon_size_rank(size: u32) -> (u8, u32) {
    if size == SCALABLE {
        (0, 0)
    } else if size >= ICON_TARGET {
        (1, size)
    } else {
        (2, u32::MAX - size)
    }
}

/// Scan one theme root (`…/icons/hicolor`-shaped): size directories
/// (`48x48`, `scalable`, `symbolic`) each holding category directories.
/// Direct existence checks only — no full tree walk.
fn search_theme_root(root: &Path, name: &str, best: &mut Option<(u32, PathBuf)>) {
    let Ok(sizes) = std::fs::read_dir(root) else {
        return;
    };
    for size_entry in sizes.flatten() {
        let size_name = size_entry.file_name();
        let Some(size_name) = size_name.to_str() else {
            continue;
        };
        let (size, ext) = match size_name {
            "scalable" | "symbolic" => (SCALABLE, "svg"),
            s => match s.split_once('x').and_then(|(w, _)| w.parse::<u32>().ok()) {
                Some(n) => (n, "png"),
                None => continue,
            },
        };
        let Ok(categories) = std::fs::read_dir(size_entry.path()) else {
            continue;
        };
        for cat in categories.flatten() {
            let p = cat.path().join(format!("{name}.{ext}"));
            if p.is_file() {
                consider(best, size, p);
            }
        }
    }
}

fn xdg_data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    {
        dirs.push(home);
    }
    let system = std::env::var_os("XDG_DATA_DIRS")
        .and_then(|v| v.into_string().ok())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(
        system
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    );
    dirs
}

fn load_icon_file(path: &Path) -> Option<TrayIcon> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("svg") => {
            let text = std::fs::read_to_string(path).ok()?;
            match SvgIcon::parse(&text) {
                Ok(svg) => Some(TrayIcon::Svg(svg)),
                Err(err) => {
                    tracing::debug!(path = %path.display(), ?err, "svg parse failed");
                    None
                }
            }
        }
        Some("png") => load_png(path).map(TrayIcon::Raster),
        _ => None,
    }
}

fn load_png(path: &Path) -> Option<Image> {
    let file = std::fs::File::open(path).ok()?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    // Expands indexed/low-bit-depth images to 8-bit channels.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = match decoder.read_info() {
        Ok(r) => r,
        Err(err) => {
            tracing::debug!(path = %path.display(), %err, "png decode failed");
            return None;
        }
    };
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|px| [px[0], px[1], px[2], 0xff])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|px| [px[0], px[0], px[0], px[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 0xff]).collect(),
        other => {
            tracing::debug!(path = %path.display(), ?other, "unexpected png color type");
            return None;
        }
    };
    Some(Image::from_rgba8(info.width, info.height, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_forms() {
        let a = Address::parse(":1.42", None).unwrap();
        assert_eq!(
            (a.bus.as_str(), a.path.as_str()),
            (":1.42", "/StatusNotifierItem")
        );

        let b = Address::parse(":1.42/org/ayatana/NotificationItem/nm", None).unwrap();
        assert_eq!(
            (b.bus.as_str(), b.path.as_str()),
            (":1.42", "/org/ayatana/NotificationItem/nm")
        );

        let c = Address::parse("/StatusNotifierItem", Some(":1.7")).unwrap();
        assert_eq!(
            (c.bus.as_str(), c.path.as_str()),
            (":1.7", "/StatusNotifierItem")
        );

        assert!(Address::parse("/StatusNotifierItem", None).is_none());
    }

    #[test]
    fn access_key_stripping() {
        assert_eq!(strip_access_key("_File"), "File");
        assert_eq!(strip_access_key("Save _As"), "Save As");
        assert_eq!(strip_access_key("a__b"), "a_b");
        assert_eq!(strip_access_key("plain"), "plain");
    }

    #[test]
    fn pixmap_argb_to_rgba() {
        // One blue pixel, 50% alpha: ARGB32 network order = [A,R,G,B].
        let icon = best_pixmap(vec![(1, 1, vec![0x80, 0x00, 0x00, 0xff])]).unwrap();
        let TrayIcon::Raster(img) = icon else {
            panic!("expected raster");
        };
        assert_eq!(img.pixels(), &[0x00, 0x00, 0xff, 0x80]);
    }

    #[test]
    fn pixmap_size_choice() {
        // 22 < target, 48 ≥ target, 64 ≥ target → choose 48.
        let mk = |s: i32| (s, s, vec![0u8; (s * s * 4) as usize]);
        let TrayIcon::Raster(img) = best_pixmap(vec![mk(22), mk(64), mk(48)]).unwrap() else {
            panic!("expected raster");
        };
        assert_eq!(img.width(), 48);
    }
}
