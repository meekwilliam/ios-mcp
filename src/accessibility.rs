use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use idevice::{
    Idevice, IdeviceError, ReadWrite, RsdService,
    dvt::message::{Aux, AuxValue, Message, MessageHeader, PayloadHeader},
};
use image::RgbImage;
use plist::{Dictionary, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf},
    sync::{Mutex, mpsc},
};

use crate::{Error, Result, control::Phones, phone};

// a busy screen has a few dozen elements, past this it's a long list the agent should scroll instead
const MOST: usize = 60;
const BUDGET: Duration = Duration::from_secs(45);

const PREVIOUS: i64 = 3;
const NEXT: i64 = 4;
const FIRST: i64 = 5;

// what xcode's accessibility inspector tells the daemon, which keeps app tracking off until it hears a version
const HOST_API_VERSION: i64 = 7;

#[derive(Debug, Clone)]
pub struct Element {
    pub label: String,
    pub role: String,
    pub value: String,
    /// x, y, width, height in screenshot points
    pub frame: Option<(u32, u32, u32, u32)>,
}

type Writer = Arc<Mutex<WriteHalf<Box<dyn ReadWrite>>>>;

/// the accessibility inspector's daemon, the same one xcode's accessibility inspector talks to, over a small dtx
/// client of our own since idevice's can't answer the daemon's questions
struct Inspector {
    writer: Writer,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    sent: u32,
}

struct Incoming {
    message: Message,
    reply: bool,
}

impl RsdService for Inspector {
    fn rsd_service_name() -> std::borrow::Cow<'static, str> {
        "com.apple.accessibility.axAuditDaemon.remoteserver.shim.remote".into()
    }

    async fn from_stream(stream: Box<dyn ReadWrite>) -> std::result::Result<Self, IdeviceError> {
        let mut device = Idevice::new(stream, "ios-mcp");
        device.rsd_checkin().await?;
        let socket = device.get_socket().ok_or(IdeviceError::NoEstablishedConnection)?;
        let (reader, writer) = tokio::io::split(socket);
        let writer = Arc::new(Mutex::new(writer));
        let (sender, incoming) = mpsc::unbounded_channel();
        // reading on its own task, so giving up on a wait never leaves half a message in the socket
        tokio::spawn(read_all(reader, writer.clone(), sender));
        Ok(Inspector {
            writer,
            incoming,
            sent: 0,
        })
    }
}

async fn read_all(mut reader: ReadHalf<Box<dyn ReadWrite>>, writer: Writer, sender: mpsc::UnboundedSender<Incoming>) {
    while let Ok((message, id, conversation, wants_reply)) = read_one(&mut reader).await {
        if wants_reply {
            let answer = match &message.data {
                Some(Value::String(s)) if s == "hostApiVersion" => Some(Value::Integer(HOST_API_VERSION.into())),
                _ => None,
            };
            if send_reply(&writer, id, conversation, answer).await.is_err() {
                break;
            }
        }
        let reply = conversation > 0;
        if sender.send(Incoming { message, reply }).is_err() {
            break;
        }
    }
}

/// one whole message, its id, conversation index, and whether it wants an answer, which idevice's parser keeps private
async fn read_one(reader: &mut ReadHalf<Box<dyn ReadWrite>>) -> std::io::Result<(Message, u32, u32, bool)> {
    let mut raw = Vec::new();
    loop {
        let mut header = [0u8; 32];
        reader.read_exact(&mut header).await?;
        raw.extend_from_slice(&header);
        let u16_at = |i: usize| u16::from_le_bytes([header[i], header[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(header[i..i + 4].try_into().expect("4 bytes"));
        let (fragment, fragments) = (u16_at(8), u16_at(10));
        // the first of several fragments is only a header
        if fragments > 1 && fragment == 0 {
            continue;
        }
        let mut body = vec![0u8; u32_at(12) as usize];
        reader.read_exact(&mut body).await?;
        raw.extend_from_slice(&body);
        if fragment + 1 >= fragments {
            let message = Message::from_reader(&mut std::io::Cursor::new(raw))
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            return Ok((message, u32_at(16), u32_at(20), u32_at(28) == 1));
        }
    }
}

/// an answer to one of the daemon's questions, or a bare acknowledgement without one
async fn send_reply(writer: &Writer, id: u32, conversation: u32, answer: Option<Value>) -> std::io::Result<()> {
    let data = match &answer {
        Some(answer) => ns_keyed_archive::encode::encode_to_bytes(answer.clone())
            .map_err(|e| std::io::Error::other(e.to_string()))?,
        None => Vec::new(),
    };
    let mut out = Vec::with_capacity(48 + data.len());
    out.extend_from_slice(&0x1F3D5B79u32.to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(16 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&(conversation + 1).to_le_bytes());
    // channel 0, so its sign doesn't matter
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    // 3 is a reply with an object in it, 0 a plain ok
    out.extend_from_slice(&[if answer.is_some() { 3 } else { 0 }, 0, 0, 0]);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&data);
    let mut writer = writer.lock().await;
    writer.write_all(&out).await?;
    writer.flush().await
}

struct Focus {
    caption: String,
    handle: Vec<u8>,
}

impl Inspector {
    async fn invoke(&mut self, selector: &str, args: Vec<Value>, wants_reply: bool) -> Result<()> {
        self.sent += 1;
        let aux =
            (!args.is_empty()).then(|| Aux::from_values(args.into_iter().map(AuxValue::archived_value).collect()));
        let header = MessageHeader::new(0, 1, self.sent, 0, 0, wants_reply);
        let message = Message::new(
            header,
            PayloadHeader::method_invocation(),
            aux,
            Some(Value::String(selector.into())),
        );
        let mut writer = self.writer.lock().await;
        writer.write_all(&message.serialize()).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn send(&mut self, selector: &str, args: Vec<Value>) -> Result<()> {
        self.invoke(selector, args, false).await
    }

    async fn next(&mut self, wait: Duration) -> Option<Incoming> {
        tokio::time::timeout(wait, self.incoming.recv()).await.ok()?
    }

    /// the daemon only answers for elements the inspector has focused this session
    async fn attribute(&mut self, handle: &[u8], name: &str) -> Result<Value> {
        let element = typed(
            "AXAuditElement_v1",
            [("PlatformElementValue_v1", Value::Data(handle.to_vec()))],
        );
        let attribute = typed(
            "AXAuditElementAttribute_v1",
            [("AttributeNameValue_v1", Value::String(name.into()))],
        );
        self.invoke("deviceElement:valueForAttribute:", vec![element, attribute], true)
            .await?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Some(incoming) = self.next(left).await else { break };
            if incoming.reply {
                return Ok(incoming
                    .message
                    .data
                    .as_ref()
                    .map(unwrap)
                    .unwrap_or(Value::String(String::new())));
            }
        }
        Err(Error::Device(IdeviceError::Timeout))
    }

    /// moves the inspector's cursor and waits to hear where it landed
    async fn step(&mut self, direction: i64) -> Result<Option<Focus>> {
        let options = passthrough(Value::Dictionary(Dictionary::from_iter([
            ("allowNonAX".to_string(), passthrough(0.into())),
            ("direction".to_string(), passthrough(direction.into())),
            ("includeContainers".to_string(), passthrough(1.into())),
        ])));
        self.send("deviceInspectorMoveWithOptions:", vec![options]).await?;
        let deadline = Instant::now() + Duration::from_millis(1500);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Some(Incoming { message, .. }) = self.next(left).await else {
                break;
            };
            if message.data != Some(Value::String("hostInspectorCurrentElementChanged:".into())) {
                continue;
            }
            let Some(focus) = message.aux.iter().flat_map(|aux| &aux.values).find_map(focus) else {
                continue;
            };
            return Ok(Some(focus));
        }
        Ok(None)
    }
}

/// lists what's on the phone's screen by stepping the accessibility inspector through it, the way voiceover would,
/// the daemon never says where an element is, so its frame comes from the highlight the inspector draws around it
pub async fn describe(phones: &mut Phones, udid: &str) -> Result<Vec<Element>> {
    let (mut inspector, _tunnel) = phone::service::<Inspector>(udid).await?;
    // the daemon says hello first, answering before then gets dropped
    while inspector.next(Duration::from_millis(400)).await.is_some() {}
    inspector.send("deviceDidGetTargeted", vec![]).await?;
    // the daemon remembers the last app it was pointed at, even one that has since closed, 0 means whatever's in front
    inspector.send("deviceSetAuditTargetPid:", vec![0.into()]).await?;
    inspector
        .send("deviceSetAppMonitoringEnabled:", vec![true.into()])
        .await?;
    inspector
        .send("deviceInspectorSetMonitoredEventType:", vec![0.into()])
        .await?;
    inspector.send("deviceInspectorShowVisuals:", vec![0.into()]).await?;
    while inspector.next(Duration::from_millis(400)).await.is_some() {}
    let clean = phones.pixels(udid).await?;
    inspector.send("deviceInspectorShowVisuals:", vec![1.into()]).await?;
    let found = walk(&mut inspector, phones, udid, &clean).await;
    let _ = inspector.send("deviceInspectorShowVisuals:", vec![0.into()]).await;
    let _ = inspector
        .send("deviceSetAppMonitoringEnabled:", vec![false.into()])
        .await;
    // let the last two calls reach the phone before the tunnel closes
    let _ = inspector.next(Duration::from_millis(200)).await;
    let mut found = found?;
    found.sort_by_key(|e| e.frame.map(|(x, y, _, _)| (y, x)).unwrap_or((u32::MAX, u32::MAX)));
    Ok(found)
}

async fn walk(inspector: &mut Inspector, phones: &mut Phones, udid: &str, clean: &RgbImage) -> Result<Vec<Element>> {
    let started = Instant::now();
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    let mut last_frame = None;
    // first, then next until it wraps or runs out, and if first doesn't answer, next then previous from wherever
    // the cursor was left
    let mut direction = FIRST;
    while found.len() < MOST && started.elapsed() < BUDGET {
        let Some(focus) = inspector.step(direction).await? else {
            direction = match direction {
                FIRST => NEXT,
                NEXT if found.is_empty() => break,
                NEXT => PREVIOUS,
                _ => break,
            };
            continue;
        };
        if direction == FIRST {
            direction = NEXT;
        }
        if !seen.insert(focus.handle.clone()) {
            if direction == NEXT {
                break;
            }
            continue;
        }
        let label = text(&inspector.attribute(&focus.handle, "Label").await?);
        let role = text(&inspector.attribute(&focus.handle, "TraitsHumanReadable").await?);
        let value = text(&inspector.attribute(&focus.handle, "Value").await?);
        let frame = highlight(phones, udid, clean, last_frame).await?;
        last_frame = frame.or(last_frame);
        found.push(Element {
            label: if label.is_empty() { focus.caption } else { label },
            role,
            value,
            frame,
        });
    }
    Ok(found)
}

/// waits for the inspector's highlight to settle somewhere new and reads its box off the screen
async fn highlight(
    phones: &mut Phones,
    udid: &str,
    clean: &RgbImage,
    previous: Option<(u32, u32, u32, u32)>,
) -> Result<Option<(u32, u32, u32, u32)>> {
    let mut last = None;
    // the highlight slides from the last element to this one, so read it once the screen stops changing
    for _ in 0..2 {
        phones.settle(udid, Duration::from_millis(1500)).await?;
        let now = outline(clean, &phones.pixels(udid).await?);
        if now.is_some() && now != previous {
            return Ok(now);
        }
        last = now;
    }
    Ok(last.filter(|frame| Some(*frame) != previous))
}

/// the box around the biggest ring of changed pixels, which is the highlight's border
fn outline(clean: &RgbImage, now: &RgbImage) -> Option<(u32, u32, u32, u32)> {
    if clean.dimensions() != now.dimensions() {
        return None;
    }
    let (width, height) = clean.dimensions();
    let changed: Vec<bool> = clean
        .pixels()
        .zip(now.pixels())
        .map(|(a, b)| (0..3).map(|c| a[c].abs_diff(b[c])).max().unwrap_or(0) > 40)
        .collect();
    let mut visited = vec![false; changed.len()];
    let mut best: Option<((u32, u32, u32, u32), u32)> = None;
    for start in 0..changed.len() {
        if !changed[start] || visited[start] {
            continue;
        }
        let mut stack = vec![start];
        visited[start] = true;
        let mut pixels = Vec::new();
        while let Some(i) = stack.pop() {
            pixels.push(i);
            let (x, y) = ((i as u32) % width, (i as u32) / width);
            // a gap of a pixel or two in the border still counts as joined
            for dy in -2i32..=2 {
                for dx in -2i32..=2 {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    if nx < 0 || ny < 0 || nx >= width as i32 || ny >= height as i32 {
                        continue;
                    }
                    let n = ny as usize * width as usize + nx as usize;
                    if changed[n] && !visited[n] {
                        visited[n] = true;
                        stack.push(n);
                    }
                }
            }
        }
        let xs = pixels.iter().map(|&i| i as u32 % width);
        let ys = pixels.iter().map(|&i| i as u32 / width);
        let (left, right) = (xs.clone().min()?, xs.max()?);
        let (top, bottom) = (ys.clone().min()?, ys.max()?);
        let (w, h) = (right - left + 1, bottom - top + 1);
        if w < 8 || h < 8 {
            continue;
        }
        // a ring keeps its pixels near its edges, a changed clock digit or picture fills its middle
        let near_edge = pixels
            .iter()
            .filter(|&&i| {
                let (x, y) = (i as u32 % width, i as u32 / width);
                x - left < 4 || right - x < 4 || y - top < 4 || bottom - y < 4
            })
            .count();
        if near_edge * 10 < pixels.len() * 7 {
            continue;
        }
        let area = w * h;
        if best.is_none_or(|(_, a)| area > a) {
            best = Some(((left, top, w, h), area));
        }
    }
    best.map(|(frame, _)| frame)
}

fn passthrough(value: Value) -> Value {
    Value::Dictionary(Dictionary::from_iter([
        ("ObjectType".to_string(), Value::String("passthrough".into())),
        ("Value".to_string(), value),
    ]))
}

/// the daemon's own objects, a type name around a dictionary of passthrough fields
fn typed<const N: usize>(kind: &str, fields: [(&str, Value); N]) -> Value {
    let fields = Dictionary::from_iter(fields.into_iter().map(|(k, v)| (k.to_string(), passthrough(v))));
    Value::Dictionary(Dictionary::from_iter([
        ("ObjectType".to_string(), Value::String(kind.into())),
        ("Value".to_string(), passthrough(Value::Dictionary(fields))),
    ]))
}

/// strips the daemon's object wrappers down to plain values
fn unwrap(value: &Value) -> Value {
    match value {
        Value::Dictionary(d) if d.contains_key("ObjectType") => {
            d.get("Value").map(unwrap).unwrap_or(Value::Boolean(false))
        }
        Value::Dictionary(d) => Value::Dictionary(d.iter().map(|(k, v)| (k.clone(), unwrap(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(unwrap).collect()),
        other => other.clone(),
    }
}

fn focus(aux: &AuxValue) -> Option<Focus> {
    let AuxValue::Array(bytes) = aux else { return None };
    let value = unwrap(&ns_keyed_archive::decode::from_bytes(bytes).ok()?);
    let fields = match &value {
        Value::Array(a) => a.first()?.as_dictionary()?,
        other => other.as_dictionary()?,
    };
    let handle = fields
        .get("ElementValue_v1")?
        .as_dictionary()?
        .get("PlatformElementValue_v1")?
        .as_data()?
        .to_vec();
    let caption = fields
        .get("CaptionTextValue_v1")
        .and_then(Value::as_string)
        .unwrap_or_default()
        .to_string();
    Some(Focus { caption, handle })
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        Value::Integer(n) => n.to_string(),
        Value::Real(n) => n.to_string(),
        _ => String::new(),
    }
}
