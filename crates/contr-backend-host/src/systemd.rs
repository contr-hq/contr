//! systemd manager calls over D-Bus.

use zbus::proxy::{Builder, CacheProperties, MethodFlags};
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, Proxy};

use crate::Bus;

const DESTINATION: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";
const UNIT_INTERFACE: &str = "org.freedesktop.systemd1.Unit";

pub(crate) struct Systemd {
    connection: Connection,
    manager: Proxy<'static>,
}

impl Systemd {
    pub(crate) async fn connect(bus: Bus) -> zbus::Result<Self> {
        let connection = match bus {
            Bus::System => Connection::system().await?,
            Bus::Session => Connection::session().await?,
        };
        let manager = Builder::new(&connection)
            .destination(DESTINATION)?
            .path(MANAGER_PATH)?
            .interface(MANAGER_INTERFACE)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;

        Ok(Self { connection, manager })
    }

    /// Whether a unit file for `unit` exists.
    pub(crate) async fn is_installed(&self, unit: &str) -> zbus::Result<bool> {
        let path: OwnedObjectPath = self.manager.call("LoadUnit", &(unit,)).await?;
        let unit_proxy: Proxy<'_> = Builder::new(&self.connection)
            .destination(DESTINATION)?
            .path(path)?
            .interface(UNIT_INTERFACE)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let state: String = unit_proxy.get_property("LoadState").await?;

        Ok(state == "loaded")
    }

    /// Queues a start job; it completes after this returns.
    pub(crate) async fn start(&self, unit: &str) -> zbus::Result<()> {
        self.queue_job("StartUnit", unit).await
    }

    /// Queues a stop job; it completes after this returns.
    pub(crate) async fn stop(&self, unit: &str) -> zbus::Result<()> {
        self.queue_job("StopUnit", unit).await
    }

    async fn queue_job(&self, method: &'static str, unit: &str) -> zbus::Result<()> {
        // System units need polkit authorization; the flag lets an agent prompt instead of refusing outright.
        self.manager
            .call_with_flags::<_, _, OwnedObjectPath>(
                method,
                MethodFlags::AllowInteractiveAuth.into(),
                &(unit, "replace"),
            )
            .await?;
        Ok(())
    }
}
