//! The watchdog's reset: unbind the adapter's driver and bind it again
//! through sysfs, which makes the kernel reinitialize a controller that
//! stopped answering.
//!
//! Linux only; `watch` is included for Linux builds.

use std::{
    fs, io,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

/// The names of one rebind, for the log.
pub struct Rebind {
    /// The driver's name, for example `hci_uart_bcm`.
    pub driver: String,
    /// The device's name in the driver's bind and unbind files, for
    /// example `serial0-0`.
    pub device: String,
}

/// Finds the driver directory and the device name of adapter `hci` under
/// `sysfs`. `Ok(None)` when the adapter has no driver link, as with vhci.
pub fn find(sysfs: &Path, hci: &str) -> io::Result<Option<(PathBuf, String)>> {
    let device = fs::canonicalize(sysfs.join("class/bluetooth").join(hci).join("device"))?;
    let driver = match fs::canonicalize(device.join("driver")) {
        Ok(driver) => driver,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let name = device
        .file_name()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the adapter's device path has no name",
            )
        })?
        .to_string_lossy()
        .into_owned();
    Ok(Some((driver, name)))
}

/// Unbinds adapter `hci`'s driver and binds it again, pausing between the
/// two writes; the watchdog pauses 2 s. `Ok(None)` when there is no driver
/// to rebind.
pub fn rebind(sysfs: &Path, hci: &str, pause: Duration) -> io::Result<Option<Rebind>> {
    let Some((driver_dir, device)) = find(sysfs, hci)? else {
        return Ok(None);
    };
    rebind_at(&driver_dir, &device, pause)?;
    let driver = driver_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Some(Rebind { driver, device }))
}

/// Writes `device` to the driver's unbind and bind files. The driver
/// directory was resolved before unbinding because the device's `driver`
/// link disappears on unbind.
fn rebind_at(driver_dir: &Path, device: &str, pause: Duration) -> io::Result<()> {
    fs::write(driver_dir.join("unbind"), device)?;
    thread::sleep(pause);
    fs::write(driver_dir.join("bind"), device)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs, io,
        os::unix::fs::symlink,
        path::{Path, PathBuf},
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    /// A fake sysfs tree in the temp directory, removed when the test ends.
    struct Tree(PathBuf);

    impl Tree {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("blessh-reset-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Builds the fake sysfs tree: a serial adapter's device directory,
    /// the hci_uart_bcm driver with its bind and unbind files, and the
    /// class link that names the adapter `hci0`. Without `driver`, the
    /// device has no driver link (the vhci case).
    fn tree(driver: bool) -> Tree {
        let tree = Tree::new();
        let root = tree.path();
        let device = root.join("devices/platform/serial0/serial0-0");
        let driver_dir = root.join("bus/serial/drivers/hci_uart_bcm");
        fs::create_dir_all(&device).unwrap();
        fs::create_dir_all(&driver_dir).unwrap();
        fs::write(driver_dir.join("bind"), "").unwrap();
        fs::write(driver_dir.join("unbind"), "").unwrap();
        fs::create_dir_all(root.join("class/bluetooth/hci0")).unwrap();
        symlink(
            "../../../devices/platform/serial0/serial0-0",
            root.join("class/bluetooth/hci0/device"),
        )
        .unwrap();
        if driver {
            symlink(
                "../../../../bus/serial/drivers/hci_uart_bcm",
                device.join("driver"),
            )
            .unwrap();
        }
        tree
    }

    #[test]
    fn finds_driver_and_device() {
        let tree = tree(true);
        let (driver, device) = find(tree.path(), "hci0").unwrap().unwrap();
        assert!(
            driver.ends_with("bus/serial/drivers/hci_uart_bcm"),
            "{}",
            driver.display()
        );
        assert_eq!(device, "serial0-0");
    }

    #[test]
    fn rebind_writes_unbind_then_bind() {
        let tree = tree(true);
        let done = rebind(tree.path(), "hci0", Duration::ZERO)
            .unwrap()
            .unwrap();
        assert_eq!(done.driver, "hci_uart_bcm");
        assert_eq!(done.device, "serial0-0");
        let driver = tree.path().join("bus/serial/drivers/hci_uart_bcm");
        for file in ["unbind", "bind"] {
            let written = fs::read_to_string(driver.join(file)).unwrap();
            assert!(written.contains("serial0-0"), "{file}: {written:?}");
        }
    }

    #[test]
    fn rebind_uses_driver_resolved_before_unbind() {
        let tree = tree(true);
        let (driver, device) = find(tree.path(), "hci0").unwrap().unwrap();
        // Unbinding removes the device's driver link, so binding must not
        // need it: the driver directory was resolved before unbinding.
        fs::remove_file(
            tree.path()
                .join("devices/platform/serial0/serial0-0/driver"),
        )
        .unwrap();
        rebind_at(&driver, &device, Duration::ZERO).unwrap();
        let bind = tree.path().join("bus/serial/drivers/hci_uart_bcm/bind");
        let written = fs::read_to_string(bind).unwrap();
        assert!(written.contains("serial0-0"), "{written:?}");
    }

    #[test]
    fn no_driver_is_not_resettable() {
        let tree = tree(false);
        assert!(find(tree.path(), "hci0").unwrap().is_none());
    }

    #[test]
    fn missing_adapter_is_an_error() {
        let tree = tree(true);
        let err = find(tree.path(), "hci7").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
