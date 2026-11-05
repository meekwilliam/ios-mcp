use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::{
    Error, Result, apps, keys,
    picture::{self, Picture},
    screen::{Button, Input, Session},
};

// a session keeps the phone's screen recording indicator on, so it closes once the agent goes quiet
const IDLE: Duration = Duration::from_secs(120);

struct Live {
    session: Session,
    used: Instant,
    size: Option<(u32, u32)>,
}

#[derive(Default)]
pub struct Phones {
    live: HashMap<String, Live>,
}

impl Phones {
    async fn live(&mut self, udid: &str) -> Result<&mut Live> {
        if self.live.get(udid).is_some_and(|l| !l.session.alive()) {
            self.live.remove(udid);
        }
        if !self.live.contains_key(udid) {
            let session = Session::start(udid).await?;
            self.live.insert(
                udid.to_string(),
                Live {
                    session,
                    used: Instant::now(),
                    size: None,
                },
            );
        }
        let live = self.live.get_mut(udid).expect("just added");
        live.used = Instant::now();
        Ok(live)
    }

    pub async fn screenshot(&mut self, udid: &str) -> Result<Picture> {
        let live = self.live(udid).await?;
        let frame = live.session.key_frame().await?;
        let picture = tokio::task::spawn_blocking(move || picture::from_key_frame(&frame))
            .await
            .map_err(|e| Error::Other(e.to_string()))??;
        live.size = Some((picture.width, picture.height));
        Ok(picture)
    }

    /// screenshot coordinates to the 0 to 65535 range the phone takes
    async fn to_phone(&mut self, udid: &str, x: f64, y: f64) -> Result<(u16, u16)> {
        let size = match self.live(udid).await?.size {
            Some(size) => size,
            None => {
                let picture = self.screenshot(udid).await?;
                (picture.width, picture.height)
            }
        };
        let (width, height) = (size.0 as f64, size.1 as f64);
        if !(0.0..=width).contains(&x) || !(0.0..=height).contains(&y) {
            return Err(Error::Other(format!(
                "({x}, {y}) is off the screen, which runs from (0, 0) to ({width}, {height})."
            )));
        }
        let scale = |v: f64, size: f64| ((v / size).clamp(0.0, 1.0) * 65535.0).round() as u16;
        Ok((scale(x, width), scale(y, height)))
    }

    pub async fn tap(&mut self, udid: &str, x: f64, y: f64, hold: Duration) -> Result<()> {
        let (x, y) = self.to_phone(udid, x, y).await?;
        let session = &self.live(udid).await?.session;
        session.send(Input::Touch { x, y })?;
        tokio::time::sleep(hold).await;
        session.send(Input::Lift { x, y })?;
        settle().await;
        Ok(())
    }

    pub async fn swipe(&mut self, udid: &str, from: (f64, f64), to: (f64, f64), duration: Duration) -> Result<()> {
        let start = self.to_phone(udid, from.0, from.1).await?;
        let end = self.to_phone(udid, to.0, to.1).await?;
        let session = &self.live(udid).await?.session;
        // one move per screen refresh, so the phone sees a smooth drag it can fling from
        let steps = (duration.as_millis() / 16).clamp(2, 200) as u32;
        let step = duration / steps;
        let at = |i: u32| {
            let t = i as f64 / steps as f64;
            let mix = |a: u16, b: u16| (a as f64 + (b as f64 - a as f64) * t).round() as u16;
            (mix(start.0, end.0), mix(start.1, end.1))
        };
        for i in 0..=steps {
            let (x, y) = at(i);
            session.send(Input::Touch { x, y })?;
            tokio::time::sleep(step).await;
        }
        session.send(Input::Lift { x: end.0, y: end.1 })?;
        settle().await;
        Ok(())
    }

    pub async fn press(&mut self, udid: &str, button: Button) -> Result<()> {
        self.live(udid).await?.session.send(Input::Button(button))?;
        tokio::time::sleep(Duration::from_millis(600)).await;
        Ok(())
    }

    /// types on the phone's keyboard, text a us keyboard can't type goes through the clipboard and gets pasted
    pub async fn type_text(&mut self, udid: &str, text: &str) -> Result<()> {
        let presses = match keys::presses(text) {
            Some(presses) => presses,
            None => {
                apps::copy_text(udid, text).await?;
                vec![
                    (keys::COMMAND, true),
                    (keys::V, true),
                    (keys::V, false),
                    (keys::COMMAND, false),
                ]
            }
        };
        let session = &self.live(udid).await?.session;
        for (usage, down) in presses {
            session.send(Input::Key { usage, down })?;
            // without a gap some apps drop keys while they're still drawing the last one
            if !down {
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        }
        settle().await;
        Ok(())
    }

    pub async fn close_idle(&mut self) {
        let idle: Vec<String> = self
            .live
            .iter()
            .filter(|(_, l)| l.used.elapsed() > IDLE || !l.session.alive())
            .map(|(udid, _)| udid.clone())
            .collect();
        for udid in idle {
            if let Some(live) = self.live.remove(&udid) {
                live.session.stop().await;
            }
        }
    }

    pub async fn close_all(&mut self) {
        for (_, live) in self.live.drain() {
            live.session.stop().await;
        }
    }
}

// lets the phone finish reacting so a screenshot straight after shows the result
async fn settle() {
    tokio::time::sleep(Duration::from_millis(400)).await;
}
