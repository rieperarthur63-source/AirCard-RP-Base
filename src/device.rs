use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::ptr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::apple::{
    AMDServiceConnectionRef, AMDeviceNotificationCallbackInfo, AMDeviceNotificationRef,
    AMDeviceRef, AppleLibraries, get_apple_libraries,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DeviceTransport {
    Usb,
    Wifi,
    Other,
}

impl DeviceTransport {
    fn from_usbmux(value: &str) -> Self {
        if value.eq_ignore_ascii_case("USB") {
            Self::Usb
        } else if value.eq_ignore_ascii_case("Network") {
            Self::Wifi
        } else {
            Self::Other
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Usb => "USB",
            Self::Wifi => "WiFi",
            Self::Other => "Other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ConnectionMode {
    Auto,
    Usb,
    Wifi,
}

impl ConnectionMode {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Usb, Self::Wifi];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto (USB preferred)",
            Self::Usb => "USB only",
            Self::Wifi => "WiFi only",
        }
    }

    fn accepts(self, transport: DeviceTransport) -> bool {
        match self {
            Self::Auto => true,
            Self::Usb => transport == DeviceTransport::Usb,
            Self::Wifi => transport == DeviceTransport::Wifi,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeviceInfo {
    pub udid: String,
    pub name: String,
    pub product_type: String,
    pub ios_version: String,
    pub build_version: String,
    pub transports: Vec<DeviceTransport>,
}

impl DeviceInfo {
    pub fn has_transport(&self, transport: DeviceTransport) -> bool {
        self.transports.contains(&transport)
    }

    pub fn supports(&self, mode: ConnectionMode) -> bool {
        self.transports.iter().copied().any(|transport| mode.accepts(transport))
    }

    pub fn transport_summary(&self) -> String {
        self.transports
            .iter()
            .map(|transport| transport.label())
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

impl std::fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}, iOS {} [{}]) [{}]",
            self.name,
            self.product_type,
            self.ios_version,
            self.build_version,
            self.transport_summary(),
        )
    }
}

#[derive(Clone)]
pub struct UsbmuxDeviceEntry {
    pub udid: String,
    pub transport: DeviceTransport,
    pub properties_plist: Vec<u8>,
}


struct NotificationListContext {
    libs: Arc<AppleLibraries>,
    devices: Vec<DeviceInfo>,
}

fn read_device_string(
    libs: &AppleLibraries,
    device: AMDeviceRef,
    key: &str,
    fallback: &str,
) -> String {
    let Ok(cf_key) = libs.create_cf_string(key) else {
        return fallback.to_string();
    };
    let value = unsafe { (libs.am_device_copy_value)(device, ptr::null(), cf_key.raw) };
    if value.is_null() {
        return fallback.to_string();
    }
    let text = libs.to_rust_string(value);
    unsafe { (libs.cf_release)(value) };
    if text.is_empty() {
        fallback.to_string()
    } else {
        text
    }
}

extern "C" fn enumerate_notification_device(
    info: *const AMDeviceNotificationCallbackInfo,
    context: *mut c_void,
) {
    if info.is_null() || context.is_null() {
        return;
    }

    let info = unsafe { &*info };
    if info.device.is_null() || info.message != 1 {
        return;
    }

    let context = unsafe { &mut *(context as *mut NotificationListContext) };
    let libs = &context.libs;

    let identifier = unsafe { (libs.am_device_copy_device_identifier)(info.device) };
    if identifier.is_null() {
        return;
    }
    let udid = libs.to_rust_string(identifier);
    unsafe { (libs.cf_release)(identifier) };
    if udid.is_empty()
        || context
            .devices
            .iter()
            .any(|device| device.udid.eq_ignore_ascii_case(&udid))
    {
        return;
    }

    let mut device_info = DeviceInfo {
        udid,
        name: "iPhone".to_string(),
        product_type: "iPhone".to_string(),
        ios_version: "Unknown".to_string(),
        build_version: "Unknown".to_string(),
        transports: vec![DeviceTransport::Usb],
    };

    unsafe {
        if (libs.am_device_connect)(info.device) == 0 {
            if (libs.am_device_is_paired)(info.device) == 0 {
                let _ = (libs.am_device_pair)(info.device);
            }

            let mut pairing_status = (libs.am_device_validate_pairing)(info.device);
            if pairing_status != 0 {
                let _ = (libs.am_device_pair)(info.device);
                pairing_status = (libs.am_device_validate_pairing)(info.device);
            }

            if pairing_status == 0 && (libs.am_device_start_session)(info.device) == 0 {
                device_info.name =
                    read_device_string(libs, info.device, "DeviceName", "iPhone");
                device_info.product_type =
                    read_device_string(libs, info.device, "ProductType", "iPhone");
                device_info.ios_version =
                    read_device_string(libs, info.device, "ProductVersion", "Unknown");
                device_info.build_version =
                    read_device_string(libs, info.device, "BuildVersion", "Unknown");
                (libs.am_device_stop_session)(info.device);
            }

            (libs.am_device_disconnect)(info.device);
        }
    }

    context.devices.push(device_info);
}

fn run_notification_enumeration(libs: Arc<AppleLibraries>) -> Result<Vec<DeviceInfo>> {
    let mut context = NotificationListContext {
        libs: Arc::clone(&libs),
        devices: Vec::new(),
    };
    let mut subscription: AMDeviceNotificationRef = ptr::null();

    let status = unsafe {
        (libs.am_device_notification_subscribe)(
            enumerate_notification_device,
            0,
            0,
            &mut context as *mut NotificationListContext as *mut c_void,
            &mut subscription,
            ptr::null(),
        )
    };
    if status != 0 {
        bail!("AMDeviceNotificationSubscribeWithOptions failed with code {}", status);
    }

    if let Ok(mode) = libs.create_cf_string("kCFRunLoopDefaultMode") {
        unsafe {
            (libs.cf_run_loop_run_in_mode)(mode.raw, 2.0, 0);
        }
    }

    if !subscription.is_null() {
        unsafe {
            (libs.am_device_notification_unsubscribe)(subscription);
        }
    }

    Ok(context.devices)
}

struct NotificationFindContext {
    libs: Arc<AppleLibraries>,
    target_udid: Option<String>,
    device: AMDeviceRef,
    udid: Option<String>,
}

extern "C" fn find_notification_device(
    info: *const AMDeviceNotificationCallbackInfo,
    context: *mut c_void,
) {
    if info.is_null() || context.is_null() {
        return;
    }

    let info = unsafe { &*info };
    if info.device.is_null() || info.message != 1 {
        return;
    }

    let context = unsafe { &mut *(context as *mut NotificationFindContext) };
    if !context.device.is_null() {
        return;
    }

    let identifier = unsafe { (context.libs.am_device_copy_device_identifier)(info.device) };
    if identifier.is_null() {
        return;
    }
    let udid = context.libs.to_rust_string(identifier);
    unsafe { (context.libs.cf_release)(identifier) };
    if udid.is_empty() {
        return;
    }

    let matches = context
        .target_udid
        .as_ref()
        .map(|target| target.eq_ignore_ascii_case(&udid))
        .unwrap_or(true);
    if !matches {
        return;
    }

    context.device = unsafe { (context.libs.cf_retain)(info.device) as AMDeviceRef };
    context.udid = Some(udid);

    let run_loop = unsafe { (context.libs.cf_run_loop_get_main)() };
    if !run_loop.is_null() {
        unsafe { (context.libs.cf_run_loop_stop)(run_loop) };
    }
}

fn find_device_via_notifications(
    libs: Arc<AppleLibraries>,
    target_udid: Option<&str>,
) -> Result<Option<(AMDeviceRef, String)>> {
    let mut context = NotificationFindContext {
        libs: Arc::clone(&libs),
        target_udid: target_udid.map(str::to_string),
        device: ptr::null(),
        udid: None,
    };
    let mut subscription: AMDeviceNotificationRef = ptr::null();

    let status = unsafe {
        (libs.am_device_notification_subscribe)(
            find_notification_device,
            0,
            0,
            &mut context as *mut NotificationFindContext as *mut c_void,
            &mut subscription,
            ptr::null(),
        )
    };
    if status != 0 {
        bail!("AMDeviceNotificationSubscribeWithOptions failed with code {}", status);
    }

    if let Ok(mode) = libs.create_cf_string("kCFRunLoopDefaultMode") {
        unsafe {
            (libs.cf_run_loop_run_in_mode)(mode.raw, 3.0, 0);
        }
    }

    if !subscription.is_null() {
        unsafe {
            (libs.am_device_notification_unsubscribe)(subscription);
        }
    }

    match (context.device.is_null(), context.udid) {
        (false, Some(udid)) => Ok(Some((context.device, udid))),
        _ => Ok(None),
    }
}

pub fn query_usbmux_devices() -> Result<Vec<UsbmuxDeviceEntry>> {
    let addr: SocketAddr = "127.0.0.1:27015".parse().unwrap();
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .context("Could not connect to Apple Mobile Device Service (usbmuxd) at 127.0.0.1:27015. Please ensure iTunes or Apple Mobile Device Support is installed and the service is running.")?;

    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;

    let mut req_dict = HashMap::new();
    req_dict.insert("MessageType".to_string(), plist::Value::String("ListDevices".to_string()));
    req_dict.insert("ClientVersionString".to_string(), plist::Value::String("aircard".to_string()));
    req_dict.insert("ProgName".to_string(), plist::Value::String("aircard".to_string()));

    let mut plist_bytes = Vec::new();
    plist::to_writer_xml(&mut plist_bytes, &plist::Value::Dictionary(req_dict.into_iter().collect()))
        .context("Failed to serialize ListDevices request")?;

    let length = (plist_bytes.len() + 16) as u32;
    let version = 1u32;
    let msg_type = 8u32; // PLIST
    let tag = 1u32;

    let mut header = Vec::with_capacity(16);
    header.extend_from_slice(&length.to_le_bytes());
    header.extend_from_slice(&version.to_le_bytes());
    header.extend_from_slice(&msg_type.to_le_bytes());
    header.extend_from_slice(&tag.to_le_bytes());

    stream.write_all(&header)?;
    stream.write_all(&plist_bytes)?;
    stream.flush()?;

    // Read 16-byte response header
    let mut resp_header = [0u8; 16];
    stream.read_exact(&mut resp_header)?;

    let resp_len = u32::from_le_bytes([resp_header[0], resp_header[1], resp_header[2], resp_header[3]]) as usize;
    if resp_len < 16 {
        bail!("Invalid usbmux response length: {}", resp_len);
    }

    let mut payload = vec![0u8; resp_len - 16];
    stream.read_exact(&mut payload)?;

    let val = plist::Value::from_reader(std::io::Cursor::new(payload))
        .context("Failed to parse usbmux ListDevices response plist")?;

    let root_dict = val.as_dictionary().context("Expected dictionary in usbmux response")?;
    let device_list = root_dict.get("DeviceList").and_then(|v| v.as_array()).context("Expected DeviceList array in usbmux response")?;

    let mut result = Vec::new();
    for entry in device_list {
        if let Some(d) = entry.as_dictionary() {
            if let Some(props_val) = d.get("Properties") {
                if let Some(props_dict) = props_val.as_dictionary() {
                    let serial = props_dict
                        .get("SerialNumber")
                        .and_then(|v| v.as_string())
                        .unwrap_or_default()
                        .to_string();
                    let connection_type = props_dict
                        .get("ConnectionType")
                        .and_then(|v| v.as_string())
                        .unwrap_or("USB");
                    let transport = DeviceTransport::from_usbmux(connection_type);

                    let mut props_binary = Vec::new();
                    plist::to_writer_binary(&mut props_binary, props_val)
                        .context("Failed to serialize device properties to binary plist")?;

                    result.push(UsbmuxDeviceEntry {
                        udid: serial,
                        transport,
                        properties_plist: props_binary,
                    });
                }
            }
        }
    }

    Ok(result)
}

pub fn list_connected_devices() -> Result<Vec<DeviceInfo>> {
    let libs = get_apple_libraries()?;
    let mut result = Vec::new();
    let mut mux_error: Option<anyhow::Error> = None;

    match query_usbmux_devices() {
        Ok(entries) => {
            for entry in entries {
                let cf_props = libs.create_cf_plist_from_bytes(&entry.properties_plist)?;
                let dev = unsafe { (libs.am_device_create_from_properties)(cf_props.raw) };
                if dev.is_null() {
                    continue;
                }

                let mut name = "iPhone".to_string();
                let mut product_type = "iPhone".to_string();
                let mut ios_version = "Unknown".to_string();
                let mut build_version = "Unknown".to_string();

                unsafe {
                    let connected = (libs.am_device_connect)(dev) == 0;
                    if connected {
                        let _ = (libs.am_device_validate_pairing)(dev);

                        name = read_device_string(&libs, dev, "DeviceName", "iPhone");
                        product_type =
                            read_device_string(&libs, dev, "ProductType", "iPhone");
                        ios_version =
                            read_device_string(&libs, dev, "ProductVersion", "Unknown");
                        build_version =
                            read_device_string(&libs, dev, "BuildVersion", "Unknown");

                        (libs.am_device_disconnect)(dev);
                    }
                    (libs.cf_release)(dev);
                }

                merge_device_info(&mut result, DeviceInfo {
                    udid: entry.udid,
                    name,
                    product_type,
                    ios_version,
                    build_version,
                    transports: vec![entry.transport],
                });
            }
        }
        Err(error) => {
            mux_error = Some(error);
        }
    }

    // Some Windows Apple Mobile Device installations expose the iPhone through
    // MobileDevice notifications even when usbmuxd's ListDevices response is empty.
    // Use the same MobileDevice notification path as the original AirCard client
    // as a USB fallback so a valid Apple driver/service stack can still work.
    if result.is_empty() {
        match run_notification_enumeration(Arc::clone(&libs)) {
            Ok(devices) => {
                for device in devices {
                    merge_device_info(&mut result, device);
                }
            }
            Err(notification_error) => {
                if let Some(mux_error) = mux_error {
                    bail!(
                        "usbmuxd scan failed: {mux_error:#}; MobileDevice fallback failed: {notification_error:#}"
                    );
                }
                return Err(notification_error);
            }
        }
    }

    if result.is_empty() {
        if let Some(mux_error) = mux_error {
            return Err(mux_error);
        }
    }

    Ok(result)
}

fn merge_device_info(devices: &mut Vec<DeviceInfo>, incoming: DeviceInfo) {
    if let Some(existing) = devices
        .iter_mut()
        .find(|device| device.udid.eq_ignore_ascii_case(&incoming.udid))
    {
        for transport in incoming.transports {
            if !existing.transports.contains(&transport) {
                existing.transports.push(transport);
            }
        }
        existing.transports.sort_by_key(|transport| transport_priority(*transport));

        if existing.name == "iPhone" && incoming.name != "iPhone" {
            existing.name = incoming.name;
        }
        if existing.product_type == "iPhone" && incoming.product_type != "iPhone" {
            existing.product_type = incoming.product_type;
        }
        if existing.ios_version == "Unknown" && incoming.ios_version != "Unknown" {
            existing.ios_version = incoming.ios_version;
        }
        if existing.build_version == "Unknown" && incoming.build_version != "Unknown" {
            existing.build_version = incoming.build_version;
        }
        return;
    }

    devices.push(incoming);
}

fn transport_priority(transport: DeviceTransport) -> u8 {
    match transport {
        DeviceTransport::Usb => 0,
        DeviceTransport::Wifi => 1,
        DeviceTransport::Other => 2,
    }
}

fn ordered_candidates(
    mut entries: Vec<UsbmuxDeviceEntry>,
    target_udid: Option<&str>,
    mode: ConnectionMode,
) -> Vec<UsbmuxDeviceEntry> {
    entries.retain(|entry| {
        target_udid
            .map(|target| entry.udid.eq_ignore_ascii_case(target))
            .unwrap_or(true)
            && mode.accepts(entry.transport)
    });
    entries.sort_by_key(|entry| transport_priority(entry.transport));
    entries
}

pub fn ensure_transport_available(
    udid: &str,
    transport: DeviceTransport,
) -> Result<()> {
    let available = query_usbmux_devices()?.into_iter().any(|entry| {
        entry.udid.eq_ignore_ascii_case(udid) && entry.transport == transport
    });
    if available {
        return Ok(());
    }

    bail!(
        "iPhone {} is no longer available over {}. Refresh devices and reconnect before retrying.",
        udid,
        transport.label()
    )
}

#[allow(dead_code)]
pub struct ActiveDeviceSession {
    pub libs: Arc<AppleLibraries>,
    pub device: AMDeviceRef,
    pub udid: String,
    pub transport: DeviceTransport,
    connected: bool,
    session_started: bool,
}

impl Drop for ActiveDeviceSession {
    fn drop(&mut self) {
        unsafe {
            if self.session_started {
                (self.libs.am_device_stop_session)(self.device);
            }
            if self.connected {
                (self.libs.am_device_disconnect)(self.device);
            }
            if !self.device.is_null() {
                (self.libs.cf_release)(self.device);
            }
        }
    }
}

impl ActiveDeviceSession {
    pub fn open(target_udid: Option<&str>, mode: ConnectionMode) -> Result<Self> {
        let libs = get_apple_libraries()?;
        let entries = query_usbmux_devices().unwrap_or_default();
        let candidates = ordered_candidates(entries, target_udid, mode);

        let mut failures = Vec::new();
        for entry in candidates {
            let transport = entry.transport;
            match Self::open_entry(Arc::clone(&libs), entry) {
                Ok(session) => return Ok(session),
                Err(err) => failures.push(format!("{}: {err:#}", transport.label())),
            }
        }

        if mode != ConnectionMode::Wifi {
            match find_device_via_notifications(Arc::clone(&libs), target_udid) {
                Ok(Some((device, udid))) => {
                    match Self::open_device_ref(
                        Arc::clone(&libs),
                        device,
                        udid,
                        DeviceTransport::Usb,
                    ) {
                        Ok(session) => return Ok(session),
                        Err(err) => failures.push(format!("MobileDevice USB fallback: {err:#}")),
                    }
                }
                Ok(None) => {}
                Err(err) => failures.push(format!("MobileDevice USB fallback scan: {err:#}")),
            }
        }

        let target = target_udid.unwrap_or("any paired iPhone");
        if failures.is_empty() {
            bail!(
                "No {} connection is available for {}. Unlock the iPhone, tap Trust, then reconnect USB and refresh.",
                mode.label(),
                target
            );
        }

        bail!(
            "Could not open iPhone session for {}. {}",
            target,
            failures.join("; ")
        )
    }

    fn open_entry(libs: Arc<AppleLibraries>, entry: UsbmuxDeviceEntry) -> Result<Self> {
        let udid = entry.udid.clone();
        let transport = entry.transport;
        let cf_props = libs.create_cf_plist_from_bytes(&entry.properties_plist)?;

        let device = unsafe { (libs.am_device_create_from_properties)(cf_props.raw) };
        if device.is_null() {
            bail!("AMDeviceCreateFromProperties failed");
        }

        Self::open_device_ref(libs, device, udid, transport)
    }

    fn open_device_ref(
        libs: Arc<AppleLibraries>,
        device: AMDeviceRef,
        udid: String,
        transport: DeviceTransport,
    ) -> Result<Self> {
        unsafe {
            let connect_status = (libs.am_device_connect)(device);
            if connect_status != 0 {
                (libs.cf_release)(device);
                bail!("AMDeviceConnect failed with code {}", connect_status);
            }

            if (libs.am_device_is_paired)(device) == 0 {
                if transport == DeviceTransport::Wifi {
                    (libs.am_device_disconnect)(device);
                    (libs.cf_release)(device);
                    bail!("WiFi device is not paired. Connect it over USB once and trust this computer first");
                }
                (libs.am_device_pair)(device);
            }

            let mut validate_status = (libs.am_device_validate_pairing)(device);
            if validate_status != 0 && transport == DeviceTransport::Usb {
                (libs.am_device_pair)(device);
                validate_status = (libs.am_device_validate_pairing)(device);
            }
            if validate_status != 0 {
                (libs.am_device_disconnect)(device);
                (libs.cf_release)(device);
                bail!(
                    "AMDeviceValidatePairing failed with code {} over {}. Unlock the iPhone and trust this computer, then retry.",
                    validate_status,
                    transport.label()
                );
            }

            let session_status = (libs.am_device_start_session)(device);
            if session_status != 0 {
                (libs.am_device_disconnect)(device);
                (libs.cf_release)(device);
                bail!("AMDeviceStartSession failed with code {}", session_status);
            }

            Ok(Self {
                libs,
                device,
                udid,
                transport,
                connected: true,
                session_started: true,
            })
        }
    }

    pub fn start_service(&self, service_name: &str) -> Result<AMDServiceConnectionRef> {
        let cf_name = self.libs.create_cf_string(service_name)?;
        let mut service_conn: AMDServiceConnectionRef = ptr::null_mut();
        let status = unsafe {
            (self.libs.am_device_secure_start_service)(
                self.device,
                cf_name.raw,
                ptr::null(),
                &mut service_conn,
            )
        };
        if status != 0 || service_conn.is_null() {
            bail!("AMDeviceSecureStartService('{}') failed with code {}", service_name, status);
        }
        Ok(service_conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(udid: &str, transport: DeviceTransport) -> UsbmuxDeviceEntry {
        UsbmuxDeviceEntry {
            udid: udid.to_string(),
            transport,
            properties_plist: Vec::new(),
        }
    }

    #[test]
    fn test_connection_mode_candidate_order() {
        let entries = vec![
            entry("phone", DeviceTransport::Wifi),
            entry("other", DeviceTransport::Usb),
            entry("phone", DeviceTransport::Usb),
        ];

        let auto = ordered_candidates(entries.clone(), Some("phone"), ConnectionMode::Auto);
        assert_eq!(auto.len(), 2);
        assert_eq!(auto[0].transport, DeviceTransport::Usb);
        assert_eq!(auto[1].transport, DeviceTransport::Wifi);

        let wifi = ordered_candidates(entries, Some("phone"), ConnectionMode::Wifi);
        assert_eq!(wifi.len(), 1);
        assert_eq!(wifi[0].transport, DeviceTransport::Wifi);
    }

    #[test]
    fn test_merge_device_transports() {
        let mut devices = vec![DeviceInfo {
            udid: "phone".to_string(),
            name: "iPhone".to_string(),
            product_type: "iPhone".to_string(),
            ios_version: "Unknown".to_string(),
            build_version: "Unknown".to_string(),
            transports: vec![DeviceTransport::Wifi],
        }];

        merge_device_info(
            &mut devices,
            DeviceInfo {
                udid: "PHONE".to_string(),
                name: "LeeSa's iPhone".to_string(),
                product_type: "iPhone17,1".to_string(),
                ios_version: "18.6".to_string(),
                build_version: "22G86".to_string(),
                transports: vec![DeviceTransport::Usb],
            },
        );

        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "LeeSa's iPhone");
        assert_eq!(
            devices[0].transports,
            vec![DeviceTransport::Usb, DeviceTransport::Wifi]
        );
    }

    #[test]
    fn test_usbmux_query() {
        match query_usbmux_devices() {
            Ok(devs) => {
                println!("Detected {} usbmux device(s)", devs.len());
                if !devs.is_empty() {
                    println!("Detected usbmux device: {}", devs[0].udid);
                }
            }
            Err(e) => {
                println!("usbmuxd not running on this host (expected in CI): {e}");
            }
        }
    }

    #[test]
    fn test_list_connected_devices() {
        match list_connected_devices() {
            Ok(devs) => {
                for d in &devs {
                    println!("Connected iPhone: {}", d);
                }
            }
            Err(e) => {
                println!("Apple Mobile Device Support not installed on this host (expected in CI): {e}");
            }
        }
    }
}
