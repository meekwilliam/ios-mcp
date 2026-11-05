use std::{fmt, future::Future, path::PathBuf, time::Duration};

use idevice::{
    IdeviceError, IdeviceService, RsdService,
    amfi::AmfiClient,
    core_device_proxy::CoreDeviceProxy,
    lockdown::LockdownClient,
    mobile_image_mounter::ImageMounter,
    provider::{IdeviceProvider, RsdProvider},
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
};
use serde::Serialize;

const LABEL: &str = "ios-mcp";

// ios 26 accepts the screen request but never sends a frame
pub const FIRST_IOS: u64 = 27;

const DDI_URL: &str =
    "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/main/PersonalizedImages/Xcode_iOS_DDI_Personalized";
const DDI_FILES: [&str; 3] = ["Image.dmg", "Image.dmg.trustcache", "BuildManifest.plist"];

#[derive(Debug)]
pub enum Error {
    NoPhone,
    NotFound(String),
    Device(IdeviceError),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoPhone => f.write_str(
                "No iPhone found. Plug it in with a cable, unlock it, and tap Trust if it asks. On Windows, install Apple Devices from the Microsoft Store first.",
            ),
            Error::NotFound(udid) => write!(f, "Can't reach the iPhone {udid}. Unlock it, or plug it in."),
            Error::Device(IdeviceError::DeviceLocked) => f.write_str(
                "Unlock the iPhone. After a restart, it has to be unlocked once to load its developer tools.",
            ),
            Error::Device(IdeviceError::DeveloperModeNotEnabled) => {
                f.write_str("Turn on Developer Mode on the iPhone: Settings › Privacy & Security › Developer Mode.")
            }
            Error::Device(IdeviceError::InvalidHostID) => {
                f.write_str("This iPhone doesn't trust this computer yet. Plug it in, unlock it, and tap Trust.")
            }
            Error::Device(IdeviceError::Timeout) => f.write_str("The iPhone stopped answering. Unlock it, or plug it in."),
            Error::Device(e) => write!(f, "{e}"),
            Error::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {}

impl From<IdeviceError> for Error {
    fn from(e: IdeviceError) -> Self {
        Error::Device(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Other(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Link {
    Usb,
    WiFi,
}

#[derive(Debug, Clone, Serialize)]
pub struct Phone {
    pub udid: String,
    pub name: String,
    pub model: Option<String>,
    pub link: Link,
    pub ios: Option<String>,
    pub developer_mode: Option<bool>,
}

pub async fn phones() -> Result<Vec<Phone>> {
    let mut found: Vec<Phone> = Vec::new();
    for device in devices().await? {
        let link = match device.connection_type {
            Connection::Usb => Link::Usb,
            Connection::Network(_) => Link::WiFi,
            Connection::Unknown(_) => continue,
        };
        if let Some(phone) = found.iter_mut().find(|p| p.udid == device.udid) {
            if link == Link::Usb {
                phone.link = Link::Usb;
            }
            continue;
        }
        let provider = device.to_provider(UsbmuxdAddr::default(), LABEL);
        let name = lockdown_string(&provider, "DeviceName")
            .await
            .unwrap_or_else(|| format!("iPhone {}", short_udid(&device.udid)));
        let developer_mode = developer_mode(&provider).await;
        // ios hides the developer mode switch in settings until a developer tool has connected
        if developer_mode == Some(false)
            && let Ok(mut amfi) = AmfiClient::connect(&provider).await
        {
            let _ = amfi.reveal_developer_mode_option_in_ui().await;
        }
        found.push(Phone {
            udid: device.udid,
            name,
            model: lockdown_string(&provider, "ProductType").await,
            link,
            ios: lockdown_string(&provider, "ProductVersion").await,
            developer_mode,
        });
    }
    found.sort_by_key(|p| p.name.to_lowercase());
    Ok(found)
}

/// the phone to use when none is named, the only one plugged in, or the one on a cable
pub async fn pick(udid: Option<&str>) -> Result<String> {
    if let Some(udid) = udid.filter(|u| !u.is_empty()) {
        return Ok(udid.to_string());
    }
    let mut all = devices().await?;
    all.sort_by_key(|d| !matches!(d.connection_type, Connection::Usb));
    all.into_iter().next().map(|d| d.udid).ok_or(Error::NoPhone)
}

/// a provider for the phone, by cable when it's on both
pub async fn provider(udid: &str) -> Result<impl IdeviceProvider> {
    let mut matching: Vec<UsbmuxdDevice> = devices().await?.into_iter().filter(|d| d.udid == udid).collect();
    matching.sort_by_key(|d| !matches!(d.connection_type, Connection::Usb));
    let device = matching
        .into_iter()
        .next()
        .ok_or_else(|| Error::NotFound(udid.to_string()))?;
    Ok(device.to_provider(UsbmuxdAddr::default(), LABEL))
}

/// whether the phone is locked right now, lockdown's PasswordProtected flips as it locks and unlocks
pub async fn locked(udid: &str) -> bool {
    let Ok(provider) = provider(udid).await else { return false };
    lockdown_value_in(&provider, "PasswordProtected", None)
        .await
        .ok()
        .and_then(|v| v.as_boolean())
        .unwrap_or(false)
}

pub async fn ios_major(provider: &dyn IdeviceProvider) -> Option<(u64, String)> {
    let ios = lockdown_string(provider, "ProductVersion").await?;
    let major = ios.split('.').next().and_then(|m| m.parse::<u64>().ok()).unwrap_or(0);
    Some((major, ios))
}

/// opens apple's developer tunnel and makes sure the developer tools are loaded on the phone
pub async fn developer_tunnel(provider: &dyn IdeviceProvider) -> Result<(AdapterHandle, RsdHandshake)> {
    let loaded = load_developer_tools(provider).await?;
    let (mut adapter, mut handshake) = tunnel(provider).await?;
    if !loaded {
        load_developer_tools_tunneled(provider, &mut adapter, &mut handshake).await?;
    }
    Ok((adapter, handshake))
}

/// connects to one service on the phone over a fresh developer tunnel, the tunnel has to outlive the service
pub async fn service<S: RsdService>(udid: &str) -> Result<(S, (AdapterHandle, RsdHandshake))> {
    let provider = provider(udid).await?;
    let (mut adapter, mut handshake) = developer_tunnel(&provider).await?;
    let service = S::connect_rsd(&mut adapter, &mut handshake).await?;
    Ok((service, (adapter, handshake)))
}

async fn tunnel(provider: &dyn IdeviceProvider) -> Result<(AdapterHandle, RsdHandshake)> {
    let proxy = CoreDeviceProxy::connect(provider).await?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let mut adapter = proxy.create_software_tunnel()?.to_async_handle();
    let stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(stream).await?;
    Ok((adapter, handshake))
}

pub async fn within(seconds: u64, action: impl Future<Output = Result<()>>) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(seconds), action)
        .await
        .unwrap_or(Err(Error::Device(IdeviceError::Timeout)))
}

pub fn short_udid(udid: &str) -> &str {
    &udid[udid.len().saturating_sub(4)..]
}

async fn devices() -> Result<Vec<UsbmuxdDevice>> {
    let mut usbmuxd = UsbmuxdConnection::default().await.map_err(|_| Error::NoPhone)?;
    Ok(usbmuxd.get_devices().await?)
}

async fn lockdown_value_in(provider: &dyn IdeviceProvider, key: &str, domain: Option<&str>) -> Result<plist::Value> {
    let mut lockdown = LockdownClient::connect(provider).await?;
    match lockdown.get_value(Some(key), domain).await {
        Ok(v) => Ok(v),
        Err(_) => {
            lockdown.start_session(&provider.get_pairing_file().await?).await?;
            Ok(lockdown.get_value(Some(key), domain).await?)
        }
    }
}

async fn lockdown_string(provider: &dyn IdeviceProvider, key: &str) -> Option<String> {
    lockdown_value_in(provider, key, None)
        .await
        .ok()?
        .as_string()
        .map(str::to_string)
}

async fn developer_mode(provider: &dyn IdeviceProvider) -> Option<bool> {
    lockdown_value_in(provider, "DeveloperModeStatus", Some("com.apple.security.mac.amfi"))
        .await
        .ok()?
        .as_boolean()
}

fn has_developer_tools(mounted: &[plist::Value]) -> bool {
    // lookup_image("Personalized") answers NotFound even while the image is mounted, so read the mount list
    mounted.iter().any(|image| {
        image
            .as_dictionary()
            .and_then(|d| d.get("DiskImageType"))
            .and_then(plist::Value::as_string)
            == Some("Personalized")
    })
}

async fn chip_id(provider: &dyn IdeviceProvider) -> Result<u64> {
    lockdown_value_in(provider, "UniqueChipID", None)
        .await?
        .as_unsigned_integer()
        .ok_or_else(|| Error::Other("The iPhone didn't report its chip ID.".into()))
}

// over usb the upload works through lockdown but the tunnel's userspace tcp times out on it, over wi-fi it's the
// other way round, so false here means try `load_developer_tools_tunneled`
async fn load_developer_tools(provider: &dyn IdeviceProvider) -> Result<bool> {
    let mut mounter = ImageMounter::connect(provider).await?;
    if has_developer_tools(&mounter.copy_devices().await?) {
        return Ok(true);
    }
    let [image, trust_cache, manifest] = developer_disk_image().await?;
    let chip_id = chip_id(provider).await?;
    match mounter
        .mount_personalized(provider, image, trust_cache, &manifest, None, chip_id)
        .await
    {
        Ok(()) => {
            eprintln!("Loaded the developer tools on the iPhone.");
            Ok(true)
        }
        Err(IdeviceError::DeviceLocked) => Err(IdeviceError::DeviceLocked.into()),
        Err(_) => Ok(false),
    }
}

async fn load_developer_tools_tunneled(
    provider: &dyn IdeviceProvider,
    adapter: &mut impl RsdProvider,
    handshake: &mut RsdHandshake,
) -> Result<()> {
    let mut mounter = ImageMounter::connect_rsd(adapter, handshake).await?;
    if has_developer_tools(&mounter.copy_devices().await?) {
        return Ok(());
    }
    let [image, trust_cache, manifest] = developer_disk_image().await?;
    let chip_id = chip_id(provider).await?;
    mounter
        .mount_personalized_rsd(adapter, handshake, image, trust_cache, &manifest, None, chip_id)
        .await?;
    eprintln!("Loaded the developer tools on the iPhone.");
    Ok(())
}

async fn developer_disk_image() -> Result<[Vec<u8>; 3]> {
    let dir = app_dir().join("DeveloperDiskImage");
    tokio::fs::create_dir_all(&dir).await?;
    let mut files: Vec<Vec<u8>> = Vec::new();
    for name in DDI_FILES {
        let path = dir.join(name);
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) if !bytes.is_empty() => bytes,
            _ => {
                let response = idevice::http::get(format!("{DDI_URL}/{name}")).await?;
                if response.status != 200 {
                    return Err(Error::Other(format!(
                        "Couldn't download Apple's developer files ({name}: HTTP {}). Check the internet connection.",
                        response.status
                    )));
                }
                tokio::fs::write(&path, &response.body).await?;
                response.body
            }
        };
        files.push(bytes);
    }
    Ok(files.try_into().expect("three files"))
}

/// %APPDATA%\ios-mcp on windows, ~/Library/Application Support/ios-mcp on a mac
pub fn app_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("ios-mcp")
}
