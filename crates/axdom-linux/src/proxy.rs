use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, proxy};

#[proxy(
    interface = "org.a11y.Bus",
    default_service = "org.a11y.Bus",
    default_path = "/org/a11y/bus"
)]
pub trait A11yBus {
    fn get_address(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.a11y.Status",
    default_service = "org.a11y.Bus",
    default_path = "/org/a11y/bus"
)]
pub trait A11yStatus {
    #[zbus(property)]
    fn is_enabled(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn set_is_enabled(&self, value: bool) -> zbus::Result<()>;

    #[zbus(property)]
    fn screen_reader_enabled(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn set_screen_reader_enabled(&self, value: bool) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.a11y.atspi.Registry",
    default_service = "org.a11y.atspi.Registry",
    default_path = "/org/a11y/atspi/registry"
)]
pub trait EventRegistry {
    fn register_event(&self, event: &str) -> zbus::Result<()>;
    fn deregister_event(&self, event: &str) -> zbus::Result<()>;
}

#[proxy(interface = "org.a11y.atspi.Accessible", assume_defaults = false)]
pub trait Accessible {
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn description(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn child_count(&self) -> zbus::Result<i32>;

    fn get_role(&self) -> zbus::Result<u32>;

    fn get_role_name(&self) -> zbus::Result<String>;

    fn get_state(&self) -> zbus::Result<Vec<u32>>;

    fn get_child_at_index(&self, index: i32) -> zbus::Result<(String, OwnedObjectPath)>;

    fn get_interfaces(&self) -> zbus::Result<Vec<String>>;
}

#[proxy(interface = "org.a11y.atspi.Component", assume_defaults = false)]
pub trait Component {
    fn get_extents(&self, coord_type: u32) -> zbus::Result<(i32, i32, i32, i32)>;

    fn grab_focus(&self) -> zbus::Result<bool>;
}

#[proxy(interface = "org.a11y.atspi.Action", assume_defaults = false)]
pub trait Action {
    #[zbus(property, name = "NActions")]
    fn n_actions(&self) -> zbus::Result<i32>;

    fn get_name(&self, index: i32) -> zbus::Result<String>;

    fn do_action(&self, index: i32) -> zbus::Result<bool>;
}

#[proxy(interface = "org.a11y.atspi.Text", assume_defaults = false)]
pub trait Text {
    #[zbus(property)]
    fn character_count(&self) -> zbus::Result<i32>;

    fn get_text(&self, start_offset: i32, end_offset: i32) -> zbus::Result<String>;
}

#[proxy(interface = "org.a11y.atspi.EditableText", assume_defaults = false)]
pub trait EditableText {
    fn set_text_contents(&self, new_contents: &str) -> zbus::Result<bool>;

    fn insert_text(&self, position: i32, text: &str, length: i32) -> zbus::Result<bool>;

    fn delete_text(&self, start_pos: i32, end_pos: i32) -> zbus::Result<bool>;
}

#[proxy(interface = "org.a11y.atspi.Value", assume_defaults = false)]
pub trait Value {
    #[zbus(property)]
    fn current_value(&self) -> zbus::Result<f64>;

    #[zbus(property)]
    fn set_current_value(&self, value: f64) -> zbus::Result<()>;
}

/// Tuple item returned by `org.a11y.atspi.Cache.GetItems`.
pub type AtspiCacheItem = (
    (String, OwnedObjectPath),      // object
    (String, OwnedObjectPath),      // application
    (String, OwnedObjectPath),      // parent
    Vec<(String, OwnedObjectPath)>, // children
    Vec<String>,                    // interfaces
    String,                         // name
    u32,                            // role
    String,                         // description
    Vec<u32>,                       // states
);

#[proxy(
    interface = "org.a11y.atspi.Cache",
    default_path = "/org/a11y/atspi/cache",
    assume_defaults = false
)]
pub trait Cache {
    fn get_items(&self) -> zbus::Result<Vec<AtspiCacheItem>>;
}

// AT-SPI2 objects rarely implement `org.freedesktop.DBus.Properties.GetAll` cleanly,
// so every proxy must disable zbus property caching (`CacheProperties::No`).

pub async fn accessible_proxy<'a>(
    conn: &Connection,
    dest: impl TryInto<zbus::names::BusName<'a>, Error = impl Into<zbus::Error>>,
    path: impl TryInto<zbus::zvariant::ObjectPath<'a>, Error = impl Into<zbus::Error>>,
) -> zbus::Result<AccessibleProxy<'a>> {
    AccessibleProxy::builder(conn)
        .destination(dest.try_into().map_err(Into::into)?)?
        .path(path.try_into().map_err(Into::into)?)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

pub async fn component_proxy<'a>(
    conn: &Connection,
    dest: impl TryInto<zbus::names::BusName<'a>, Error = impl Into<zbus::Error>>,
    path: impl TryInto<zbus::zvariant::ObjectPath<'a>, Error = impl Into<zbus::Error>>,
) -> zbus::Result<ComponentProxy<'a>> {
    ComponentProxy::builder(conn)
        .destination(dest.try_into().map_err(Into::into)?)?
        .path(path.try_into().map_err(Into::into)?)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

pub async fn action_proxy<'a>(
    conn: &Connection,
    dest: impl TryInto<zbus::names::BusName<'a>, Error = impl Into<zbus::Error>>,
    path: impl TryInto<zbus::zvariant::ObjectPath<'a>, Error = impl Into<zbus::Error>>,
) -> zbus::Result<ActionProxy<'a>> {
    ActionProxy::builder(conn)
        .destination(dest.try_into().map_err(Into::into)?)?
        .path(path.try_into().map_err(Into::into)?)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

pub async fn text_proxy<'a>(
    conn: &Connection,
    dest: impl TryInto<zbus::names::BusName<'a>, Error = impl Into<zbus::Error>>,
    path: impl TryInto<zbus::zvariant::ObjectPath<'a>, Error = impl Into<zbus::Error>>,
) -> zbus::Result<TextProxy<'a>> {
    TextProxy::builder(conn)
        .destination(dest.try_into().map_err(Into::into)?)?
        .path(path.try_into().map_err(Into::into)?)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

pub async fn editable_text_proxy<'a>(
    conn: &Connection,
    dest: impl TryInto<zbus::names::BusName<'a>, Error = impl Into<zbus::Error>>,
    path: impl TryInto<zbus::zvariant::ObjectPath<'a>, Error = impl Into<zbus::Error>>,
) -> zbus::Result<EditableTextProxy<'a>> {
    EditableTextProxy::builder(conn)
        .destination(dest.try_into().map_err(Into::into)?)?
        .path(path.try_into().map_err(Into::into)?)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

pub async fn value_proxy<'a>(
    conn: &Connection,
    dest: impl TryInto<zbus::names::BusName<'a>, Error = impl Into<zbus::Error>>,
    path: impl TryInto<zbus::zvariant::ObjectPath<'a>, Error = impl Into<zbus::Error>>,
) -> zbus::Result<ValueProxy<'a>> {
    ValueProxy::builder(conn)
        .destination(dest.try_into().map_err(Into::into)?)?
        .path(path.try_into().map_err(Into::into)?)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

pub async fn cache_proxy<'a>(
    conn: &Connection,
    dest: impl TryInto<zbus::names::BusName<'a>, Error = impl Into<zbus::Error>>,
) -> zbus::Result<CacheProxy<'a>> {
    CacheProxy::builder(conn)
        .destination(dest.try_into().map_err(Into::into)?)?
        .path("/org/a11y/atspi/cache")?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

pub async fn a11y_status_proxy(conn: &Connection) -> zbus::Result<A11yStatusProxy<'_>> {
    A11yStatusProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
}
