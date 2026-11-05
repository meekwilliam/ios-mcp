use std::borrow::Cow;

use futures::TryStreamExt;
use idevice::{
    IdeviceError, ReadWrite, RsdService,
    core_device::{AppServiceClient, CoreDeviceServiceClient, GENERAL_PASTEBOARD, PasteboardServiceClient},
};
use serde::Serialize;

use crate::{Error, Result, phone};

#[derive(Debug, Clone, Serialize)]
pub struct App {
    pub name: String,
    pub bundle_id: String,
    pub version: Option<String>,
    pub built_in: bool,
}

pub async fn list(udid: &str) -> Result<Vec<App>> {
    let (mut service, _tunnel) = phone::service::<AppServiceClient<Box<dyn ReadWrite>>>(udid).await?;
    // ios 27 never answers the older listapps, only the streamed list
    let listed: Vec<_> = service
        .stream_apps(false, true, false, false, true)
        .try_collect()
        .await?;
    let mut apps: Vec<App> = listed
        .into_iter()
        .filter(|a| !a.is_hidden)
        .map(|a| App {
            name: a.name,
            bundle_id: a.bundle_identifier,
            version: a.version,
            built_in: a.is_first_party && !a.is_removable,
        })
        .collect();
    apps.sort_by_key(|a| a.name.to_lowercase());
    Ok(apps)
}

/// opens an app by its bundle id, or by its name as the home screen shows it
pub async fn launch(udid: &str, app: &str) -> Result<App> {
    let apps = list(udid).await?;
    let wanted = app.trim().to_lowercase();
    let found = apps
        .iter()
        .find(|a| a.bundle_id.to_lowercase() == wanted)
        .or_else(|| apps.iter().find(|a| a.name.to_lowercase() == wanted))
        .or_else(|| apps.iter().find(|a| a.name.to_lowercase().contains(&wanted)))
        .cloned()
        .ok_or_else(|| {
            Error::Other(format!(
                "No app called \"{app}\" on the iPhone. list_apps shows what's there."
            ))
        })?;
    let (mut service, _tunnel) = phone::service::<AppServiceClient<Box<dyn ReadWrite>>>(udid).await?;
    service
        .launch_application(&found.bundle_id, &[], false, false, None, None, None)
        .await?;
    Ok(found)
}

pub async fn copy_text(udid: &str, text: &str) -> Result<()> {
    let (mut service, _tunnel) = phone::service::<PasteboardServiceClient<Box<dyn ReadWrite>>>(udid).await?;
    Ok(service.set_text(text, GENERAL_PASTEBOARD).await?)
}

const FEATURE: &str = "com.apple.coredevice.feature.simulatelocation";
const SET: &str = "com.apple.coredevice.action.setsimulatedlocation";
const CLEAR: &str = "com.apple.coredevice.action.clearsimulatedlocation";

/// the phone's own location service then holds the spot with nothing connected, until cleared or a restart
pub async fn set_location(udid: &str, latitude: f64, longitude: f64) -> Result<()> {
    if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
        return Err(Error::Other(
            "Latitude runs from -90 to 90 and longitude from -180 to 180.".into(),
        ));
    }
    let mut input = plist::Dictionary::new();
    input.insert("latitude".into(), plist::Value::Real(latitude));
    input.insert("longitude".into(), plist::Value::Real(longitude));
    locate(udid, SET, input).await
}

pub async fn clear_location(udid: &str) -> Result<()> {
    locate(udid, CLEAR, plist::Dictionary::new()).await
}

async fn locate(udid: &str, action: &str, input: plist::Dictionary) -> Result<()> {
    let (mut service, _tunnel) = phone::service::<SimulateLocation>(udid).await?;
    service.0.invoke_with_plist_action(FEATURE, input, action).await?;
    Ok(())
}

// idevice's own location client only lists scenarios, this one sets and clears
struct SimulateLocation(CoreDeviceServiceClient<Box<dyn ReadWrite>>);

impl RsdService for SimulateLocation {
    fn rsd_service_name() -> Cow<'static, str> {
        Cow::Borrowed("com.apple.coredevice.locationservice")
    }

    async fn from_stream(stream: Box<dyn ReadWrite>) -> std::result::Result<Self, IdeviceError> {
        Ok(Self(CoreDeviceServiceClient::new(stream).await?))
    }
}
