use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use idevice::{
    IdeviceError, ReadWrite, RsdService,
    core_device::{
        ButtonState, CallInfoBlob, DisplayServiceClient, IndigoHidClient, RtpPacket, TOUCHSCREEN_STATE_CONTACT,
        TOUCHSCREEN_STATE_RELEASE, UniversalHidServiceClient, build_keyframe_request, build_liveness,
        build_screen_audio_offer, build_start_audio_parameters, build_start_video_parameters, is_rtcp,
    },
    rsd::RsdHandshake,
    tcp::handle::{AdapterHandle, UdpSocketHandle},
    xpc::{Dictionary as XpcDictionary, XPCObject},
};
use tokio::sync::{mpsc, oneshot};

use crate::{Error, Result, h264, phone, picture::Picture, video};

// device hub sends 140, the phone's own bigger mask gets an unusable video setup
const CLIENT_FEATURES: u64 = 140;

const INPUT: Duration = Duration::from_secs(3);
// the phone sends a timing report every second even while the screen is still, silence past this means it's gone
const SILENT: Duration = Duration::from_secs(5);
// a still screen still comes as frames of about 0.5 KB, anything bigger means something on it changed
const CHANGE_BYTES: usize = 1_000;
// after a key frame the phone sends a few KB more touching up its quality, which isn't the screen changing
const TOUCH_UP: Duration = Duration::from_millis(400);
// past this the phone isn't going to send a key frame, so the frames since the last one get decoded instead
const KEY_FRAME_WAIT: Duration = Duration::from_millis(2500);

#[derive(Debug, Clone, Copy)]
pub enum Input {
    Touch { x: u16, y: u16 },
    Lift { x: u16, y: u16 },
    Button(Button),
    Key { usage: u8, down: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Home,
    AppSwitcher,
    Lock,
    VolumeUp,
    VolumeDown,
}

impl Button {
    pub fn named(name: &str) -> Option<Button> {
        Some(match name.to_lowercase().replace(['_', ' '], "-").as_str() {
            "home" => Button::Home,
            "app-switcher" | "switcher" => Button::AppSwitcher,
            "lock" | "power" | "side" => Button::Lock,
            "volume-up" => Button::VolumeUp,
            "volume-down" => Button::VolumeDown,
            _ => return None,
        })
    }

    // usage page, usage, hold ms, presses
    fn usage(self) -> (u64, u64, u64, usize) {
        match self {
            Button::Home => (0x0C, 0x40, 80, 1),
            Button::AppSwitcher => (0x0C, 0x40, 60, 2),
            Button::Lock => (0x0C, 0x30, 200, 1),
            Button::VolumeUp => (0x0C, 0xE9, 80, 1),
            Button::VolumeDown => (0x0C, 0xEA, 80, 1),
        }
    }
}

enum Command {
    Input(Input),
    Current(oneshot::Sender<Shot>),
    Latest(oneshot::Sender<Shot>),
}

/// frames to decode for the screen as it is, a lone key frame unless the phone wouldn't send one
struct Shot {
    number: u64,
    frames: Vec<Vec<u8>>,
}

/// a live screen sharing session with one phone, touches and keys only reach the phone while one is running
pub struct Session {
    commands: mpsc::UnboundedSender<Command>,
    thread: Option<thread::JoinHandle<()>>,
    activity: Arc<Mutex<Activity>>,
    // the png of an unchanged screen is the same, so a second screenshot of it costs nothing
    made: Mutex<Option<(u64, Vec<u8>, u32, u32)>>,
}

struct Activity {
    frames: u64,
    changed_at: Option<Instant>,
}

// a still screen keeps coming as frames of about 0.5 KB, so a quarter second with nothing bigger means it's settled
const QUIET_FOR: Duration = Duration::from_millis(250);

impl Activity {
    fn seen(&mut self, bytes: usize, key: bool) {
        self.frames += 1;
        if !key && bytes > CHANGE_BYTES {
            self.changed_at = Some(Instant::now());
        }
    }
}

impl Session {
    pub async fn start(udid: &str) -> Result<Session> {
        let (commands, received) = mpsc::unbounded_channel();
        let (ready, started) = oneshot::channel();
        let udid = udid.to_string();
        let activity = Arc::new(Mutex::new(Activity {
            frames: 0,
            changed_at: None,
        }));
        let seen = activity.clone();
        // the stream's futures aren't Send, so it gets its own thread and runtime
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let mut ready = Some(ready);
            let result = runtime.block_on(run(&udid, received, &mut ready, &seen));
            if let Err(e) = result {
                match ready.take() {
                    Some(ready) => {
                        let _ = ready.send(Err(e));
                    }
                    None => eprintln!("Screen sharing with the iPhone stopped: {e}"),
                }
            }
        });
        let session = Session {
            commands,
            thread: Some(thread),
            activity,
            made: Mutex::new(None),
        };
        match tokio::time::timeout(Duration::from_secs(30), started).await {
            Ok(Ok(Ok(()))) => Ok(session),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(Error::Other(
                "Screen sharing with the iPhone stopped before it started.".into(),
            )),
            Err(_) => Err(Error::Device(IdeviceError::Timeout)),
        }
    }

    pub fn alive(&self) -> bool {
        !self.commands.is_closed()
    }

    pub fn send(&self, input: Input) -> Result<()> {
        self.commands
            .send(Command::Input(input))
            .map_err(|_| Error::Other("Screen sharing with the iPhone stopped. Try again.".into()))
    }

    /// the frames to decode for the screen as it is now, a lone key frame unless the phone wouldn't send one, and
    /// the number of the last, which stays the same while the screen does
    pub async fn frames(&self) -> Result<(u64, Vec<Vec<u8>>)> {
        let (reply, shot) = oneshot::channel();
        let stopped = || Error::Other("Screen sharing with the iPhone stopped. Try again.".into());
        self.commands.send(Command::Current(reply)).map_err(|_| stopped())?;
        match tokio::time::timeout(KEY_FRAME_WAIT * 2, shot).await {
            Ok(Ok(shot)) => Ok((shot.number, shot.frames)),
            Ok(Err(_)) => Err(stopped()),
            Err(_) => Err(Error::Other("The iPhone didn't send a picture of its screen in time.".into())),
        }
    }

    /// every frame since the last key frame, which shows even a small change straight away, openh264 can smear a big
    /// movement but a highlight box drawn on a still screen comes out right
    pub async fn latest(&self) -> Result<Vec<Vec<u8>>> {
        let (reply, shot) = oneshot::channel();
        let stopped = || Error::Other("Screen sharing with the iPhone stopped. Try again.".into());
        self.commands.send(Command::Latest(reply)).map_err(|_| stopped())?;
        let shot = shot.await.map_err(|_| stopped())?;
        if shot.frames.is_empty() {
            return Err(Error::Other("The iPhone hasn't sent a picture of its screen yet.".into()));
        }
        Ok(shot.frames)
    }

    /// the screen as it is now, instant when it hasn't changed since the last key frame
    pub async fn picture(&self) -> Result<Picture> {
        let (number, frames) = self.frames().await?;
        if let Some((made, png, width, height)) = self.made.lock().expect("lock").as_ref()
            && *made == number
        {
            return Ok(Picture {
                png: png.clone(),
                width: *width,
                height: *height,
            });
        }
        let picture = tokio::task::spawn_blocking(move || video::picture(&frames))
            .await
            .map_err(|e| Error::Other(e.to_string()))??;
        *self.made.lock().expect("lock") = Some((number, picture.png.clone(), picture.width, picture.height));
        Ok(picture)
    }

    /// waits until the screen has stopped changing, or `longest` for screens that never do, like a playing video
    pub async fn settle(&self, longest: Duration) {
        let since = Instant::now();
        // a still screen means nothing until the phone has started reacting, an action that changes nothing shows
        // no change within this
        let react = Duration::from_millis(600);
        loop {
            tokio::time::sleep(Duration::from_millis(30)).await;
            if self
                .activity
                .lock()
                .expect("lock")
                .changed_at
                .is_some_and(|at| at > since)
            {
                break;
            }
            if since.elapsed() > react {
                return;
            }
        }
        while since.elapsed() < longest {
            if self
                .activity
                .lock()
                .expect("lock")
                .changed_at
                .is_some_and(|at| at.elapsed() > QUIET_FOR)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    /// ends the session on the phone too, so its screen recording indicator goes away
    pub async fn stop(mut self) {
        self.close();
        let Some(thread) = self.thread.take() else { return };
        // a phone that stopped answering can't hold up quitting
        for _ in 0..30 {
            if thread.is_finished() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn close(&mut self) {
        let (closed, _) = mpsc::unbounded_channel();
        drop(std::mem::replace(&mut self.commands, closed));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

async fn run(
    udid: &str,
    mut commands: mpsc::UnboundedReceiver<Command>,
    ready: &mut Option<oneshot::Sender<Result<()>>>,
    activity: &Mutex<Activity>,
) -> Result<()> {
    let provider = phone::provider(udid).await?;
    if let Some((major, ios)) = phone::ios_major(&provider).await
        && major < phone::FIRST_IOS
    {
        return Err(Error::Other(format!(
            "Seeing and controlling the screen needs iOS {} or newer. This iPhone has iOS {ios}.",
            phone::FIRST_IOS
        )));
    }
    let (mut adapter, mut handshake) = phone::developer_tunnel(&provider).await?;

    let mut screen = Screen::start(&mut adapter, &mut handshake).await?;
    let mut controls = Controls::default();
    let mut depacketizer = h264::Depacketizer::default();
    let mut feedback = Feedback::new(screen.ssrc);
    let mut alive = tokio::time::interval(Duration::from_secs(1));
    let mut expected: Option<u16> = None;
    // after a lost packet every frame is broken until the next key frame
    let mut broken = false;
    let mut setup = Setup::default();
    let mut heard = Instant::now();
    // the frames since the last key frame, starting with it, and the number of the last one
    let mut run: Vec<Vec<u8>> = Vec::new();
    let mut last = 0u64;
    let mut key_number = 0u64;
    let mut key_at = Instant::now();
    let mut changed = false;
    let mut last_change = Instant::now();
    let mut asking: Vec<(Instant, oneshot::Sender<Shot>)> = Vec::new();
    let mut check = tokio::time::interval(Duration::from_millis(100));

    let result = loop {
        tokio::select! {
            datagram = screen.video.recv() => {
                let datagram = match datagram {
                    Ok(d) => d,
                    Err(e) => break Err(e.into()),
                };
                heard = Instant::now();
                // the phone's rtcp timing reports come in on the same port, they'd read as broken video packets
                if is_rtcp(&datagram.data) {
                    continue;
                }
                let Some(packet) = RtpPacket::parse(&datagram.data) else { continue };
                feedback.phone = Some((datagram.source_port, packet.ssrc));
                if let Some(expected) = expected
                    && packet.sequence_number.wrapping_sub(expected) < 0x8000
                    && packet.sequence_number != expected
                {
                    broken = true;
                    // the depacketizer would wait for the lost packet, the tunnel keeps order so it never comes
                    depacketizer = h264::Depacketizer::default();
                    feedback.ask_for_key_frame(&screen.video, false).await;
                }
                if expected.is_none_or(|e| packet.sequence_number.wrapping_sub(e) < 0x8000) {
                    expected = Some(packet.sequence_number.wrapping_add(1));
                }
                depacketizer.push(packet.payload);
                // the marker bit closes a picture, everything before it belongs together
                if !packet.marker {
                    continue;
                }
                let mut data = depacketizer.take();
                if data.is_empty() {
                    continue;
                }
                let key = h264::has_key_frame(&data);
                if key {
                    data = setup.attach(data);
                }
                if broken && !key {
                    continue;
                }
                broken = false;
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Ok(()));
                }
                let number = {
                    let mut seen = activity.lock().expect("lock");
                    seen.seen(data.len(), key);
                    seen.frames
                };
                if key {
                    for (_, reply) in asking.drain(..) {
                        let _ = reply.send(Shot { number, frames: vec![data.clone()] });
                    }
                    run.clear();
                    changed = false;
                    key_number = number;
                    key_at = Instant::now();
                } else if data.len() > CHANGE_BYTES && key_at.elapsed() > TOUCH_UP {
                    changed = true;
                    last_change = Instant::now();
                }
                if key || !run.is_empty() {
                    run.push(data);
                    last = number;
                }
            }
            _ = check.tick() => {
                // asking as soon as the screen settles means the key frame is usually here before anyone wants it
                if changed && (last_change.elapsed() > Duration::from_millis(250) || !asking.is_empty()) {
                    feedback.ask_for_key_frame(&screen.video, false).await;
                }
                if asking.first().is_some_and(|(since, _)| since.elapsed() > KEY_FRAME_WAIT) {
                    for (_, reply) in asking.drain(..) {
                        let _ = reply.send(Shot { number: last, frames: run.clone() });
                    }
                }
            }
            _ = alive.tick() => {
                feedback.still_watching(&screen.video).await;
                // ending here lets the next request start a fresh session instead of waiting forever
                if heard.elapsed() > SILENT {
                    break Err(Error::Other("The iPhone stopped sending its screen.".into()));
                }
            }
            command = commands.recv() => match command {
                Some(Command::Input(input)) => {
                    // a stuck send would stop the video and the feedback, and the phone ends the session 15 s later
                    match tokio::time::timeout(INPUT, controls.send(input, &mut adapter, &mut handshake)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => eprintln!("Couldn't pass {input:?} to the iPhone: {e}"),
                        Err(_) => {
                            eprintln!("The iPhone didn't take {input:?} in time, reconnecting the controls.");
                            controls = Controls::default();
                        }
                    }
                    // the phone's reports pile up unread while a send is stuck, that time isn't the phone going quiet
                    heard = heard.max(Instant::now() - Duration::from_secs(1));
                }
                Some(Command::Latest(reply)) => {
                    let _ = reply.send(Shot { number: last, frames: run.clone() });
                }
                Some(Command::Current(reply)) => {
                    if !changed && !run.is_empty() {
                        let _ = reply.send(Shot { number: key_number, frames: vec![run[0].clone()] });
                    } else {
                        asking.push((Instant::now(), reply));
                        feedback.ask_for_key_frame(&screen.video, true).await;
                    }
                }
                None => break Ok(()),
            },
        }
    };
    let _ = screen.client.stop_media_stream().await;
    result
}

// idevice asks for 3600, which keeps a killed process's session, and the phone's recording indicator, going for an
// hour, a live session stays up past this because feedback reaches the phone every second
fn with_timeout(mut params: XpcDictionary) -> XpcDictionary {
    params.insert("timeout".into(), XPCObject::UInt64(15));
    params
}

// the sockets have to stay open or the phone stops sending
struct Screen {
    client: DisplayServiceClient<Box<dyn ReadWrite>>,
    _audio: UdpSocketHandle,
    video: UdpSocketHandle,
    ssrc: u32,
}

impl Screen {
    // like device hub, audio first to set up the session, video fails without it
    async fn start(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake) -> Result<Self> {
        let mut client = DisplayServiceClient::connect_rsd(adapter, handshake).await?;
        // a session a killed process left behind keeps the phone's recording indicator on for a while
        let _ = client.stop_media_stream().await;
        let audio = adapter.bind_udp(0).await?;
        let video = adapter.bind_udp(0).await?;
        let host = adapter.host_ip().to_string();
        let phone = adapter.peer_ip().to_string();

        // the phone checks this looks like a mac running device hub, these are the values a captured offer had
        let call_info = CallInfoBlob {
            call_id: 0,
            client_version: 1,
            device_type: "Mac17,7".into(),
            framework_version: "2205.3.1".into(),
            os_version: "25F71".into(),
            device_name: None,
            audio_device_uid: None,
        };
        let session = uuid::Uuid::new_v4();
        let call_id = || uuid::Uuid::new_v4().to_string().to_uppercase();

        let offer = build_screen_audio_offer(&call_id(), &call_info)?;
        let params = build_start_audio_parameters(
            &host,
            audio.local_port(),
            &phone,
            50000,
            offer,
            CLIENT_FEATURES,
            session,
        );
        client.start_media_stream(with_timeout(params)).await?;

        let ssrc = uuid::Uuid::new_v4().as_u128() as u32;
        let offer = h264::offer(&call_id(), &call_info, ssrc)?;
        let params = build_start_video_parameters(
            &host,
            video.local_port(),
            &phone,
            50001,
            offer,
            CLIENT_FEATURES,
            1,
            session,
        );
        client.start_media_stream(with_timeout(params)).await?;
        Ok(Screen {
            client,
            _audio: audio,
            video,
            ssrc,
        })
    }
}

struct Feedback {
    ssrc: u32,
    phone: Option<(u16, u32)>,
    asked: Option<Instant>,
    requests: u8,
}

impl Feedback {
    const NAME: &str = "ios-mcp";

    fn new(ssrc: u32) -> Self {
        Feedback {
            ssrc,
            phone: None,
            asked: None,
            requests: 0,
        }
    }

    async fn still_watching(&self, video: &UdpSocketHandle) {
        if let Some((port, _)) = self.phone {
            let _ = video.send_to(port, build_liveness(self.ssrc, Self::NAME, &[])).await;
        }
    }

    async fn ask_for_key_frame(&mut self, video: &UdpSocketHandle, now: bool) {
        let Some((port, media)) = self.phone else { return };
        if !now && self.asked.is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.asked = Some(Instant::now());
        self.requests = self.requests.wrapping_add(1);
        let request = build_keyframe_request(self.ssrc, Self::NAME, media, &[], self.requests);
        let _ = video.send_to(port, request).await;
    }
}

#[derive(Default)]
struct Controls {
    touch: Option<UniversalHidServiceClient<Box<dyn ReadWrite>>>,
    buttons: Option<IndigoHidClient<Box<dyn ReadWrite>>>,
}

impl Controls {
    async fn send(&mut self, input: Input, adapter: &mut AdapterHandle, handshake: &mut RsdHandshake) -> Result<()> {
        match input {
            Input::Touch { x, y } | Input::Lift { x, y } => {
                let state = match input {
                    Input::Touch { .. } => TOUCHSCREEN_STATE_CONTACT,
                    _ => TOUCHSCREEN_STATE_RELEASE,
                };
                if self.touch.is_none() {
                    self.touch = Some(UniversalHidServiceClient::connect_rsd(adapter, handshake).await?);
                }
                let touch = self.touch.as_mut().expect("just connected");
                if let Err(e) = touch.send_touchscreen(state, x, y, None).await {
                    // a dropped connection would fail every touch after it, reconnect next time
                    self.touch = None;
                    return Err(e.into());
                }
            }
            Input::Button(button) => {
                if self.buttons.is_none() {
                    self.buttons = Some(IndigoHidClient::connect_rsd(adapter, handshake).await?);
                }
                let buttons = self.buttons.as_mut().expect("just connected");
                let (page, usage, hold, presses) = button.usage();
                let pressed = async {
                    for press in 0..presses {
                        if press > 0 {
                            tokio::time::sleep(Duration::from_millis(hold)).await;
                        }
                        buttons.send_button(page, usage, ButtonState::Down).await?;
                        tokio::time::sleep(Duration::from_millis(hold)).await;
                        buttons.send_button(page, usage, ButtonState::Up).await?;
                    }
                    Ok::<(), IdeviceError>(())
                };
                if let Err(e) = pressed.await {
                    self.buttons = None;
                    return Err(e.into());
                }
            }
            Input::Key { usage, down } => {
                if self.buttons.is_none() {
                    self.buttons = Some(IndigoHidClient::connect_rsd(adapter, handshake).await?);
                }
                let keys = self.buttons.as_mut().expect("just connected");
                let state = if down { ButtonState::Down } else { ButtonState::Up };
                if let Err(e) = keys.send_keyboard(usage as u64, state).await {
                    self.buttons = None;
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }
}

// a fresh depacketizer after a loss no longer has the sps and pps to put in front of the next key frame
#[derive(Default)]
struct Setup(Vec<u8>);

impl Setup {
    fn attach(&mut self, frame: Vec<u8>) -> Vec<u8> {
        let sets: Vec<u8> = nal_units(&frame)
            .filter(|nal| matches!(nal[0] & 0x1F, 7 | 8))
            .flat_map(|nal| [&[0, 0, 0, 1][..], nal].concat())
            .collect();
        if !sets.is_empty() {
            self.0 = sets;
            return frame;
        }
        [self.0.as_slice(), &frame].concat()
    }
}

fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts.iter().skip(1).map(|&s| s - 3).chain([data.len()]).collect();
    starts.into_iter().zip(ends).filter_map(move |(start, end)| {
        // a 4 byte start code leaves its first zero on the end of the nal before
        let end = data[..end]
            .iter()
            .rposition(|&b| b != 0)
            .map_or(start, |p| p + 1)
            .max(start);
        (end > start).then(|| &data[start..end])
    })
}
