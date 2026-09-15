use std::time::Duration;

// TODO: not architecture independent but correct for arm/ppc/x86
const SIGUSR1: i32 = 10;

// busctl call org.freedesktop.systemd1 /org/freedesktop/systemd1 org.freedesktop.systemd1.Manager KillUnit ssi bluetooth.service main 10

pub fn systemd_reload_bt_config() -> Result<(), Box<dyn std::error::Error>> {
    let con = dbus::blocking::Connection::new_system()?;

    let systemd1 = con.with_proxy(
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        Duration::from_millis(1000),
    );

    // send USR1 signal to bluetoothd service to trigger config reload
    let r: Result<(), _> = systemd1.method_call(
        "org.freedesktop.systemd1.Manager",
        "KillUnit",
        ("bluetooth.service", "main", SIGUSR1),
    );
    r?;
    Ok(())
}

fn bluez_connect_device(
    con: &dbus::blocking::Connection,
    adapter_path: &str,
    mac: &[u8; 6],
) -> Result<(), Box<dyn std::error::Error>> {
    let dev_mac = format!(
        "{:02X}_{:02X}_{:02X}_{:02X}_{:02X}_{:02X}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
    let dev_path = format!("{}/dev_{}", adapter_path, dev_mac);
    println!("connect BT dev {}", dev_path);

    let device1 = con.with_proxy("org.bluez", dev_path, Duration::from_secs(10));

    // busctl call org.bluez /org/bluez/hci0/dev_$BTADDR org.bluez.Device1 Connect
    let r: Result<(), _> = device1.method_call("org.bluez.Device1", "Connect", ());
    r?;
    Ok(())
}

fn connect_devices<I, F, E>(devices: I, mut connect: F)
where
    I: IntoIterator<Item = [u8; 6]>,
    F: FnMut(&[u8; 6]) -> Result<(), E>,
    E: std::fmt::Display,
{
    for mac in devices {
        if let Err(e) = connect(&mac) {
            eprintln!(
                "Failed to connect BT dev {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}: {}",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], e
            );
        }
    }
}

pub fn bluez_connect(
    bt_info: &crate::BtInfo,
    ble_info: &[crate::BleDevice],
) -> Result<(), Box<dyn std::error::Error>> {
    let con = dbus::blocking::Connection::new_system()?;

    let adapter_mac = format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        bt_info.mac[0],
        bt_info.mac[1],
        bt_info.mac[2],
        bt_info.mac[3],
        bt_info.mac[4],
        bt_info.mac[5]
    );

    let bluez1 = con.with_proxy("org.bluez", "/org/bluez", Duration::from_secs(2));
    let introspect = bluez1.introspect()?;
    use dbus::blocking::stdintf::org_freedesktop_dbus::Introspectable;

    // println!("bluez introspect:\n{}", introspect);

    use regex::Regex;
    let re = Regex::new(r#"<node name="(hci[0-9]+)"/>"#).unwrap();

    for (_, [adapter]) in re.captures_iter(introspect.as_str()).map(|c| c.extract()) {
        let adapter_path = format!("/org/bluez/{}", adapter);

        let adapter1 = con.with_proxy("org.bluez", &adapter_path, Duration::from_secs(2));
        use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;

        // busctl get-property org.bluez /org/bluez/hci0 org.bluez.Adapter1 Address
        let bt_addr: String = adapter1.get("org.bluez.Adapter1", "Address")?;

        if bt_addr.eq(adapter_mac.as_str()) {
            let devices = bt_info
                .devices
                .iter()
                .map(|dev| dev.mac)
                .chain(ble_info.iter().map(|dev| dev.mac));
            connect_devices(devices, |mac| {
                bluez_connect_device(&con, &adapter_path, mac)
            });
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::connect_devices;

    #[test]
    fn a_failed_connection_does_not_skip_later_devices() {
        let devices = [[1, 0, 0, 0, 0, 0], [2, 0, 0, 0, 0, 0]];
        let mut attempted = Vec::new();

        connect_devices(devices, |mac| {
            attempted.push(*mac);
            if mac[0] == 1 {
                Err("device is offline")
            } else {
                Ok(())
            }
        });

        assert_eq!(attempted, devices);
    }
}
