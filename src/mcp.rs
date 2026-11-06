use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{Error, Result, accessibility, apps, control::Phones, phone, picture::Picture, screen::Button};

const PROTOCOL: &str = "2025-06-18";

const INSTRUCTIONS: &str = "Controls a real iPhone connected to this computer. Start with screenshot to see the screen. \
tap, swipe, type_text, press_button and launch_app wait for the screen to settle and send back a screenshot of the result, so there's no need to take one after them. \
Coordinates for tap and swipe are pixels in the latest screenshot (the iPhone's points, (0, 0) is the top left). \
To open an app use launch_app rather than hunting for its icon. Tap a text field before type_text. \
describe_screen lists the buttons and text on screen with where to tap them, when a screenshot leaves it unclear. \
Every tool works on the only connected iPhone unless you pass a udid from list_devices. \
When you've finished with the iPhone, call disconnect so it stops sharing its screen.";

/// a model context protocol server on stdin and stdout, for claude code, cursor, codex and other agents
pub async fn serve() -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut phones = Phones::default();
    let mut tidy = tokio::time::interval(Duration::from_secs(5));
    loop {
        let line = tokio::select! {
            line = lines.next_line() => line?,
            _ = tidy.tick() => {
                phones.close_idle().await;
                continue;
            }
        };
        let Some(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(message, &mut phones).await,
            Err(e) => Some(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": e.to_string() },
            })),
        };
        if let Some(reply) = reply {
            let mut out = serde_json::to_vec(&reply).expect("json");
            out.push(b'\n');
            stdout.write_all(&out).await?;
            stdout.flush().await?;
        }
    }
    phones.close_all().await;
    Ok(())
}

async fn handle(message: Value, phones: &mut Phones) -> Option<Value> {
    // notifications have no id and get no answer
    let id = message.get("id").cloned()?;
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL);
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "ios-mcp", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call(name, &args, phones).await {
                Ok(content) => json!({ "content": content, "isError": false }),
                Err(e) => json!({ "content": [text(e.to_string())], "isError": true }),
            }
        }
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Unknown method {method}") },
            }));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn text(s: impl Into<String>) -> Value {
    json!({ "type": "text", "text": s.into() })
}

fn image(picture: &Picture) -> Vec<Value> {
    vec![
        json!({ "type": "image", "data": BASE64.encode(&picture.png), "mimeType": "image/png" }),
        text(format!(
            "The screen is {} x {} points. Tap with x from 0 to {} and y from 0 to {}.",
            picture.width, picture.height, picture.width, picture.height
        )),
    ]
}

/// what an action did, with a screenshot of the result so the agent needn't spend a turn asking for one
async fn shown(phones: &mut Phones, udid: &str, args: &Value, said: String) -> Result<Vec<Value>> {
    let mut content = vec![text(said)];
    if args.get("screenshot").and_then(Value::as_bool) != Some(false) {
        content.extend(image(&phones.screenshot(udid).await?));
    }
    Ok(content)
}

fn number(args: &Value, key: &str) -> Result<f64> {
    args.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| Error::Other(format!("{key} is missing, it should be a number.")))
}

fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Other(format!("{key} is missing, it should be text.")))
}

async fn call(name: &str, args: &Value, phones: &mut Phones) -> Result<Vec<Value>> {
    if name == "list_devices" {
        let found = phone::phones().await?;
        if found.is_empty() {
            return Err(Error::NoPhone);
        }
        return Ok(vec![text(serde_json::to_string_pretty(&found).expect("json"))]);
    }
    if name == "disconnect" {
        let count = phones.connected();
        phones.close_all().await;
        return Ok(vec![text(if count == 0 {
            "Already disconnected from the iPhone.".to_string()
        } else {
            "Disconnected from the iPhone, it's no longer sharing its screen. Any other tool connects again."
                .to_string()
        })]);
    }
    let udid = phone::pick(args.get("udid").and_then(Value::as_str)).await?;
    let udid = udid.as_str();
    let done = |what: String| Ok(vec![text(what)]);
    match name {
        "screenshot" => Ok(image(&phones.screenshot(udid).await?)),
        "describe_screen" => {
            let found = accessibility::describe(phones, udid).await?;
            if found.is_empty() {
                return done(
                    "The iPhone didn't list anything on this screen. Some games and video apps don't describe themselves, use screenshot instead."
                        .into(),
                );
            }
            let lines: Vec<String> = found
                .iter()
                .map(|e| {
                    let mut line = format!("{:?}", e.label);
                    if !e.role.is_empty() {
                        line += &format!(" {}", e.role.to_lowercase());
                    }
                    if !e.value.is_empty() {
                        line += &format!(", value {:?}", e.value);
                    }
                    match e.frame {
                        Some((x, y, w, h)) => {
                            line += &format!(", at ({x}, {y}) size {w} x {h}, tap ({}, {})", x + w / 2, y + h / 2)
                        }
                        None => line += ", position unknown",
                    }
                    line
                })
                .collect();
            done(format!(
                "{} things on screen, top to bottom, in screenshot points:\n{}",
                found.len(),
                lines.join("\n")
            ))
        }
        "tap" => {
            let (x, y) = (number(args, "x")?, number(args, "y")?);
            let hold = args.get("hold_ms").and_then(Value::as_u64).unwrap_or(60);
            phones.tap(udid, x, y, Duration::from_millis(hold.min(10_000))).await?;
            shown(phones, udid, args, format!("Tapped ({x}, {y}).")).await
        }
        "swipe" => {
            let from = (number(args, "from_x")?, number(args, "from_y")?);
            let to = (number(args, "to_x")?, number(args, "to_y")?);
            let ms = args.get("duration_ms").and_then(Value::as_u64).unwrap_or(300);
            phones
                .swipe(udid, from, to, Duration::from_millis(ms.clamp(50, 5000)))
                .await?;
            shown(phones, udid, args, format!("Swiped from {from:?} to {to:?}.")).await
        }
        "type_text" => {
            let typed = string(args, "text")?;
            let mut typed = typed.to_string();
            if args.get("submit").and_then(Value::as_bool) == Some(true) {
                typed.push('\n');
            }
            phones.type_text(udid, &typed).await?;
            let said = if crate::keys::presses(&typed).is_none() {
                "Pasted the text, since it has characters a keyboard can't type. If the iPhone asks to allow pasting, tap Allow Paste."
                    .to_string()
            } else {
                format!("Typed {} characters.", typed.chars().count())
            };
            shown(phones, udid, args, said).await
        }
        "press_button" => {
            let named = string(args, "button")?;
            let button = Button::named(named).ok_or_else(|| {
                Error::Other("button is one of home, lock, volume_up, volume_down, app_switcher.".into())
            })?;
            phones.press(udid, button).await?;
            shown(phones, udid, args, format!("Pressed {named}.")).await
        }
        "list_apps" => {
            let all = apps::list(udid).await?;
            let lines: Vec<String> = all.iter().map(|a| format!("{}\t{}", a.name, a.bundle_id)).collect();
            done(format!("{} apps, name and bundle id:\n{}", all.len(), lines.join("\n")))
        }
        "launch_app" => {
            let app = string(args, "app")?;
            let opened = apps::launch(udid, app).await?;
            // an app that's still loading keeps the screen changing, give it longer than a tap
            phones.settle(udid, Duration::from_millis(3000)).await?;
            shown(
                phones,
                udid,
                args,
                format!("Opened {} ({}).", opened.name, opened.bundle_id),
            )
            .await
        }
        "set_location" => {
            let (latitude, longitude) = (number(args, "latitude")?, number(args, "longitude")?);
            phone::within(60, apps::set_location(udid, latitude, longitude)).await?;
            done(format!(
                "The iPhone's location is now {latitude}, {longitude}. It stays there until clear_location or a restart."
            ))
        }
        "clear_location" => {
            phone::within(60, apps::clear_location(udid)).await?;
            done("The iPhone is back on its real location.".into())
        }
        _ => Err(Error::Other(format!("There's no tool called {name}."))),
    }
}

fn tools() -> Value {
    let udid = json!({
        "type": "string",
        "description": "Which iPhone, from list_devices. Leave out when only one is connected.",
    });
    let schema = |properties: Value, required: &[&str]| {
        let mut properties = properties;
        properties["udid"] = udid.clone();
        json!({ "type": "object", "properties": properties, "required": required })
    };
    let action = |properties: Value, required: &[&str]| {
        let mut properties = properties;
        properties["screenshot"] = json!({
            "type": "boolean",
            "description": "Send back a screenshot of the result, true by default. Set false to save tokens when doing several actions in a row.",
        });
        schema(properties, required)
    };
    json!([
        {
            "name": "list_devices",
            "description": "Lists the iPhones connected to this computer by cable or Wi-Fi, with their iOS version and whether Developer Mode is on. When it's off, this also makes its switch appear in Settings › Privacy & Security › Developer Mode.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "screenshot",
            "description": "Shows the iPhone's screen as it is right now. Coordinates for tap and swipe are pixels in this image.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "describe_screen",
            "description": "Lists what's on the iPhone's screen, like buttons, text and switches, with each one's label and where to tap it in screenshot points. Slower than a screenshot (a few seconds, more on busy screens), and a box briefly hops around the phone's screen while it reads. Off-screen items in a list need a swipe first.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "tap",
            "description": "Taps the screen at a point from the latest screenshot. Set hold_ms to long-press, like 800 for a context menu.",
            "inputSchema": action(json!({
                "x": { "type": "number", "description": "Pixels from the left of the screenshot." },
                "y": { "type": "number", "description": "Pixels from the top of the screenshot." },
                "hold_ms": { "type": "integer", "description": "How long the finger stays down, 60 by default." },
            }), &["x", "y"]),
        },
        {
            "name": "swipe",
            "description": "Drags a finger across the screen, to scroll, swipe between pages or pull down. Points are screenshot pixels. To scroll down a list, drag upward, so from_y is bigger than to_y.",
            "inputSchema": action(json!({
                "from_x": { "type": "number" },
                "from_y": { "type": "number" },
                "to_x": { "type": "number" },
                "to_y": { "type": "number" },
                "duration_ms": { "type": "integer", "description": "How long the drag takes, 300 by default. Slower drags scroll less." },
            }), &["from_x", "from_y", "to_x", "to_y"]),
        },
        {
            "name": "type_text",
            "description": "Types text into whatever has focus on the iPhone, like a text field you just tapped. Works like a keyboard plugged into the phone. Text with emoji or accents goes through the iPhone's clipboard instead, replacing what was copied there.",
            "inputSchema": action(json!({
                "text": { "type": "string" },
                "submit": { "type": "boolean", "description": "Press Return after typing, to send or search." },
            }), &["text"]),
        },
        {
            "name": "press_button",
            "description": "Presses one of the iPhone's buttons. home goes to the home screen, app_switcher shows open apps, lock turns the screen off or on.",
            "inputSchema": action(json!({
                "button": { "type": "string", "enum": ["home", "lock", "volume_up", "volume_down", "app_switcher"] },
            }), &["button"]),
        },
        {
            "name": "list_apps",
            "description": "Lists the apps on the iPhone with their names and bundle ids.",
            "inputSchema": schema(json!({}), &[]),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "launch_app",
            "description": "Opens an app, by its name as on the home screen (like Notes) or its bundle id (like com.apple.mobilenotes).",
            "inputSchema": action(json!({
                "app": { "type": "string" },
            }), &["app"]),
        },
        {
            "name": "set_location",
            "description": "Makes the iPhone think it's somewhere else, for every app, until clear_location or a restart.",
            "inputSchema": schema(json!({
                "latitude": { "type": "number" },
                "longitude": { "type": "number" },
            }), &["latitude", "longitude"]),
        },
        {
            "name": "disconnect",
            "description": "Call this when you've finished with the iPhone. It ends screen sharing, so the phone's screen recording indicator goes away. A simulated location stays until clear_location.",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "clear_location",
            "description": "Puts the iPhone back on its real location.",
            "inputSchema": schema(json!({}), &[]),
        },
    ])
}
