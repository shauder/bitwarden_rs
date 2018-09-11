use rocket::Route;
use rocket_contrib::Json;

use api::JsonResult;
use auth::Headers;
use db::DbConn;

pub fn routes() -> Vec<Route> {
    routes![negotiate, websockets_err]
}

#[get("/hub")]
fn websockets_err() -> JsonResult {
    err!("'/notifications/hub' should be proxied towards the websocket server, otherwise notifications will not work. Go to the README for more info.")
}

#[post("/hub/negotiate")]
fn negotiate(_headers: Headers, _conn: DbConn) -> JsonResult {
    use crypto;
    use data_encoding::BASE64URL;

    let conn_id = BASE64URL.encode(&crypto::get_random(vec![0u8; 16]));

    // TODO: Implement transports
    // Rocket WS support: https://github.com/SergioBenitez/Rocket/issues/90
    // Rocket SSE support: https://github.com/SergioBenitez/Rocket/issues/33
    Ok(Json(json!({
        "connectionId": conn_id,
        "availableTransports":[
                {"transport":"WebSockets", "transferFormats":["Text","Binary"]},
                // {"transport":"ServerSentEvents", "transferFormats":["Text"]},
                // {"transport":"LongPolling", "transferFormats":["Text","Binary"]}
        ]
    })))
}

///
/// Websockets server
///
use std::sync::Arc;
use std::thread;

use ws::{self, util::Token, Factory, Handler, Handshake, Message, Sender, WebSocket};

use chashmap::CHashMap;
use chrono::NaiveDateTime;
use serde_json::from_str;

use db::models::{Cipher, Folder, User};

use rmpv::Value;

fn serialize(val: Value) -> Vec<u8> {
    use rmpv::encode::write_value;

    let mut buf = Vec::new();
    write_value(&mut buf, &val).expect("Error encoding MsgPack");

    // Add size bytes at the start
    // Extracted from BinaryMessageFormat.js
    let mut size = buf.len();
    let mut len_buf: Vec<u8> = Vec::new();

    loop {
        let mut size_part = size & 0x7f;
        size = size >> 7;

        if size > 0 {
            size_part = size_part | 0x80;
        }

        len_buf.push(size_part as u8);

        if size <= 0 {
            break;
        }
    }

    len_buf.append(&mut buf);
    len_buf
}

fn serialize_date(date: NaiveDateTime) -> Value {
    let seconds: i64 = date.timestamp();
    let nanos: i64 = date.timestamp_subsec_nanos() as i64;
    let timestamp = nanos << 34 | seconds;

    use byteorder::{BigEndian, WriteBytesExt};

    let mut bs = [0u8; 8];
    bs.as_mut()
        .write_i64::<BigEndian>(timestamp)
        .expect("Unable to write");

    // -1 is Timestamp
    // https://github.com/msgpack/msgpack/blob/master/spec.md#timestamp-extension-type
    Value::Ext(-1, bs.to_vec())
}

fn convert_option<T: Into<Value>>(option: Option<T>) -> Value {
    match option {
        Some(a) => a.into(),
        None => Value::Nil,
    }
}

// Server WebSocket handler
pub struct WSHandler {
    out: Sender,
    user_uuid: Option<String>,
    users: WebSocketUsers,
}

const RECORD_SEPARATOR: u8 = 0x1e;
const INITIAL_RESPONSE: [u8; 3] = [0x7b, 0x7d, RECORD_SEPARATOR]; // {, }, <RS>

#[derive(Deserialize)]
struct InitialMessage {
    protocol: String,
    version: i32,
}

const PING_MS: u64 = 15_000;
const PING: Token = Token(1);

impl Handler for WSHandler {
    fn on_open(&mut self, hs: Handshake) -> ws::Result<()> {
        // TODO: Improve this split
        let path = hs.request.resource();
        let mut query_split: Vec<_> = path.split("?").nth(1).unwrap().split("&").collect();
        query_split.sort();
        let access_token = &query_split[0][13..];
        let _id = &query_split[1][3..];

        // Validate the user
        use auth;
        let claims = match auth::decode_jwt(access_token) {
            Ok(claims) => claims,
            Err(_) => {
                return Err(ws::Error::new(
                    ws::ErrorKind::Internal,
                    "Invalid access token provided",
                ))
            }
        };

        // Assign the user to the handler
        let user_uuid = claims.sub;
        self.user_uuid = Some(user_uuid.clone());

        // Add the current Sender to the user list
        let handler_insert = self.out.clone();
        let handler_update = self.out.clone();

        self.users.map.upsert(
            user_uuid,
            || vec![handler_insert],
            |ref mut v| v.push(handler_update),
        );

        // Schedule a ping to keep the connection alive
        self.out.timeout(PING_MS, PING)
    }

    fn on_message(&mut self, msg: Message) -> ws::Result<()> {
        println!("Server got message '{}'. ", msg);

        if let Message::Text(text) = msg.clone() {
            let json = &text[..text.len() - 1]; // Remove last char

            if let Ok(InitialMessage { protocol, version }) = from_str::<InitialMessage>(json) {
                if &protocol == "messagepack" && version == 1 {
                    return self.out.send(&INITIAL_RESPONSE[..]); // Respond to initial message
                }
            }
        }

        // If it's not the initial message, just echo the message
        self.out.send(msg)
    }

    fn on_timeout(&mut self, event: Token) -> ws::Result<()> {
        if event == PING {
            // send ping
            self.out.send(create_ping())?;

            // reschedule the timeout
            self.out.timeout(PING_MS, PING)
        } else {
            Err(ws::Error::new(
                ws::ErrorKind::Internal,
                "Invalid timeout token provided",
            ))
        }
    }
}

struct WSFactory {
    pub users: WebSocketUsers,
}

impl WSFactory {
    pub fn init() -> Self {
        WSFactory {
            users: WebSocketUsers {
                map: Arc::new(CHashMap::new()),
            },
        }
    }
}

impl Factory for WSFactory {
    type Handler = WSHandler;

    fn connection_made(&mut self, out: Sender) -> Self::Handler {
        println!("WS: Connection made");
        WSHandler {
            out,
            user_uuid: None,
            users: self.users.clone(),
        }
    }

    fn connection_lost(&mut self, handler: Self::Handler) {
        println!("WS: Connection lost");

        // Remove handler
        let user_uuid = &handler.user_uuid.unwrap();
        if let Some(mut user_conn) = self.users.map.get_mut(user_uuid) {
            user_conn.remove_item(&handler.out);
        }
    }
}

#[derive(Clone)]
pub struct WebSocketUsers {
    pub map: Arc<CHashMap<String, Vec<Sender>>>,
}

impl WebSocketUsers {
    fn send_update(&self, user_uuid: &String, data: Vec<u8>) -> ws::Result<()> {
        if let Some(user) = self.map.get(user_uuid) {
            for sender in user.iter() {
                sender.send(data.clone())?;
            }
        }
        Ok(())
    }

    // NOTE: The last modified date needs to be updated before calling these methods
    pub fn send_user_update(&self, ut: UpdateType, user: &User) {
        let data = create_update(
            vec![
                ("UserId".into(), user.uuid.clone().into()),
                ("Date".into(), serialize_date(user.updated_at)),
            ].into(),
            ut,
        );

        self.send_update(&user.uuid.clone(), data).ok();
    }

    pub fn send_folder_update(&self, ut: UpdateType, folder: &Folder) {
        let data = create_update(
            vec![
                ("Id".into(), folder.uuid.clone().into()),
                ("UserId".into(), folder.user_uuid.clone().into()),
                ("RevisionDate".into(), serialize_date(folder.updated_at)),
            ].into(),
            ut,
        );

        self.send_update(&folder.user_uuid, data).ok();
    }

    pub fn send_cipher_update(&self, ut: UpdateType, cipher: &Cipher, user_uuids: &Vec<String>) {
        let user_uuid = convert_option(cipher.user_uuid.clone());
        let org_uuid = convert_option(cipher.organization_uuid.clone());

        let data = create_update(
            vec![
                ("Id".into(), cipher.uuid.clone().into()),
                ("UserId".into(), user_uuid),
                ("OrganizationId".into(), org_uuid),
                ("CollectionIds".into(), Value::Nil),
                ("RevisionDate".into(), serialize_date(cipher.updated_at)),
            ].into(),
            ut,
        );

        for uuid in user_uuids {
            self.send_update(&uuid, data.clone()).ok();
        }
    }
}

/* Message Structure
[
    1, // MessageType.Invocation
    {}, // Headers
    null, // InvocationId
    "ReceiveMessage", // Target
    [ // Arguments
        {
            "ContextId": "app_id",
            "Type": ut as i32,
            "Payload": {}
        }
    ]
]
*/
fn create_update(payload: Vec<(Value, Value)>, ut: UpdateType) -> Vec<u8> {
    use rmpv::Value as V;

    let value = V::Array(vec![
        1.into(),
        V::Array(vec![]),
        V::Nil,
        "ReceiveMessage".into(),
        V::Array(vec![V::Map(vec![
            ("ContextId".into(), "app_id".into()),
            ("Type".into(), (ut as i32).into()),
            ("Payload".into(), payload.into()),
        ])]),
    ]);

    serialize(value)
}

fn create_ping() -> Vec<u8> {
    serialize(Value::Array(vec![6.into()]))
}

#[allow(dead_code)]
pub enum UpdateType {
    SyncCipherUpdate = 0,
    SyncCipherCreate = 1,
    SyncLoginDelete = 2,
    SyncFolderDelete = 3,
    SyncCiphers = 4,

    SyncVault = 5,
    SyncOrgKeys = 6,
    SyncFolderCreate = 7,
    SyncFolderUpdate = 8,
    SyncCipherDelete = 9,
    SyncSettings = 10,

    LogOut = 11,
}

pub fn start_notification_server() -> WebSocketUsers {
    let factory = WSFactory::init();
    let users = factory.users.clone();

    thread::spawn(move || {
        WebSocket::new(factory)
            .unwrap()
            .listen("0.0.0.0:3012")
            .unwrap();
    });

    users
}
