// Shared types for compile-time benchmarking of remoc.
// These are complex, deeply nested types that stress monomorphization.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Simple {
    pub a: u8,
    pub b: u16,
    pub c: u32,
    pub d: u64,
    pub e: i8,
    pub f: i16,
    pub g: i32,
    pub h: i64,
    pub i: f32,
    pub j: f64,
    pub k: bool,
    pub l: String,
    pub m: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Color {
    Red,
    Green,
    Blue,
    Rgb(u8, u8, u8),
    Named(String),
    Custom { hue: f64, saturation: f64, value: f64 },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Address {
    pub street: String,
    pub city: String,
    pub state: String,
    pub zip: String,
    pub country: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Person {
    pub name: String,
    pub age: u32,
    pub email: Option<String>,
    pub address: Address,
    pub tags: Vec<String>,
    pub metadata: BTreeMap<String, String>,
    pub favorite_color: Color,
    pub active: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Company {
    pub name: String,
    pub employees: Vec<Person>,
    pub headquarters: Address,
    pub revenue: f64,
    pub departments: BTreeMap<String, Vec<Person>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    Structured {
        from: Person,
        to: Vec<Person>,
        subject: String,
        body: String,
        attachments: Vec<Vec<u8>>,
        priority: u8,
    },
    SystemNotification {
        code: u32,
        message: String,
        severity: Color,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Config {
    pub version: u32,
    pub name: String,
    pub entries: Vec<ConfigEntry>,
    pub nested: BTreeMap<String, Config>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ConfigEntry {
    pub key: String,
    pub value: ConfigValue,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ConfigValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    List(Vec<ConfigValue>),
    Map(BTreeMap<String, ConfigValue>),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Matrix {
    pub rows: usize,
    pub cols: usize,
    pub data: Vec<Vec<f64>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TimeSeries {
    pub name: String,
    pub points: Vec<(f64, f64)>,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Document {
    pub title: String,
    pub author: Person,
    pub sections: Vec<Section>,
    pub references: Vec<String>,
    pub metadata: BTreeMap<String, ConfigValue>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Section {
    pub heading: String,
    pub content: String,
    pub subsections: Vec<Section>,
    pub figures: Vec<Figure>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Figure {
    pub caption: String,
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Event {
    pub id: u64,
    pub timestamp: u64,
    pub kind: EventKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum EventKind {
    Login { user: Person },
    Logout { user_id: u64 },
    Purchase { item: String, amount: f64, currency: String },
    Error { code: u32, message: String, stack: Vec<String> },
    Batch(Vec<Event>),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ApiResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: ResponseBody,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ResponseBody {
    Json(String),
    Binary(Vec<u8>),
    PersonList(Vec<Person>),
    CompanyData(Company),
    EventLog(Vec<Event>),
    Config(Config),
    Error { code: u32, message: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DatabaseRecord {
    pub id: u64,
    pub table: String,
    pub fields: BTreeMap<String, ConfigValue>,
    pub relations: Vec<(String, u64)>,
    pub version: u32,
    pub deleted: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Workspace {
    pub name: String,
    pub owner: Person,
    pub documents: Vec<Document>,
    pub configs: Vec<Config>,
    pub events: Vec<Event>,
    pub records: Vec<DatabaseRecord>,
    pub responses: Vec<ApiResponse>,
    pub time_series: Vec<TimeSeries>,
    pub matrices: Vec<Matrix>,
    pub messages: Vec<Message>,
}
