mod apps;
mod control;
mod h264;
mod keys;
mod mcp;
mod phone;
mod picture;
mod screen;

use std::time::Duration;

use control::Phones;
use phone::{Error, Result};
use screen::Button;

const USAGE: &str = "ios-mcp lets AI agents see and control an iPhone.

With no arguments it runs as an MCP server on stdin and stdout. Add it to Claude Code with:
    claude mcp add iphone -- ios-mcp

Or use it from the command line:
    ios-mcp devices
    ios-mcp screenshot <file.png>
    ios-mcp tap <x> <y>
    ios-mcp swipe <from-x> <from-y> <to-x> <to-y>
    ios-mcp type <text>
    ios-mcp button home|lock|volume-up|volume-down|app-switcher
    ios-mcp apps
    ios-mcp launch <app name or bundle id>
    ios-mcp location <latitude> <longitude>
    ios-mcp location clear

Coordinates are pixels in a screenshot. Set IOS_MCP_UDID to pick an iPhone when more than one is connected.";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None | Some("mcp") | Some("serve") => mcp::serve().await,
        Some("-h") | Some("--help") | Some("help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some("-V") | Some("--version") => {
            println!("ios-mcp {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(_) => command(&args).await,
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn command(args: &[String]) -> Result<()> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let number = |s: &str| {
        s.parse::<f64>()
            .map_err(|_| Error::Other(format!("{s} isn't a number.")))
    };
    if args == ["devices"] {
        for p in phone::phones().await? {
            let developer = match p.developer_mode {
                Some(true) => "developer mode on",
                Some(false) => "developer mode off",
                None => "developer mode ?",
            };
            println!(
                "{}\t{}\t{}\tios {}\t{developer}",
                p.udid,
                p.name,
                if p.link == phone::Link::Usb { "usb" } else { "wifi" },
                p.ios.as_deref().unwrap_or("?"),
            );
        }
        return Ok(());
    }
    let udid = phone::pick(std::env::var("IOS_MCP_UDID").ok().as_deref()).await?;
    let udid = udid.as_str();
    let mut phones = Phones::default();
    let result = match args.as_slice() {
        ["screenshot", file] => match phones.screenshot(udid).await {
            Ok(picture) => {
                std::fs::write(file, &picture.png)?;
                println!("{file}: {} x {}", picture.width, picture.height);
                Ok(())
            }
            Err(e) => Err(e),
        },
        ["tap", x, y] => {
            phones
                .tap(udid, number(x)?, number(y)?, Duration::from_millis(60))
                .await
        }
        ["swipe", fx, fy, tx, ty] => {
            let from = (number(fx)?, number(fy)?);
            let to = (number(tx)?, number(ty)?);
            phones.swipe(udid, from, to, Duration::from_millis(300)).await
        }
        ["type", words @ ..] if !words.is_empty() => phones.type_text(udid, &words.join(" ")).await,
        ["button", name] => match Button::named(name) {
            Some(button) => phones.press(udid, button).await,
            None => Err(Error::Other(
                "buttons: home, lock, volume-up, volume-down, app-switcher".into(),
            )),
        },
        ["apps"] => {
            for app in apps::list(udid).await? {
                println!("{}\t{}", app.name, app.bundle_id);
            }
            Ok(())
        }
        ["launch", app @ ..] if !app.is_empty() => {
            let opened = apps::launch(udid, &app.join(" ")).await?;
            println!("opened {} ({})", opened.name, opened.bundle_id);
            Ok(())
        }
        ["location", "clear"] => phone::within(60, apps::clear_location(udid)).await,
        ["location", lat, lon] => phone::within(60, apps::set_location(udid, number(lat)?, number(lon)?)).await,
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    phones.close_all().await;
    result
}
