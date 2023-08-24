use crate::{Authentication, AuthenticationError};
use prettytable::{cell, Cell, Row};
use std::collections::HashMap;

use log::debug;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use thiserror::Error;

use reqwest::header::{HeaderMap, InvalidHeaderValue};
use reqwest::StatusCode;

pub mod asset;
pub mod edge_app;
mod edge_app_utils;
pub(crate) mod playlist;
pub mod screen;

pub enum OutputType {
    HumanReadable,
    Json,
}

pub trait Formatter {
    fn format(&self, output_type: OutputType) -> String;
}

pub trait FormatterValue {
    fn value(&self) -> &serde_json::Value;
}

// Helper function to format a value returned from the API.
// Can be used if there is no need to make any transformation on the returned value.
fn format_value<T, F>(
    output_type: OutputType,
    column_names: Vec<&str>,
    field_names: Vec<&str>,
    value: &T,
    value_transformer: Option<F>,
) -> String
where
    T: FormatterValue,
    F: Fn(&str, &serde_json::Value) -> Cell, // Takes field name and field value and returns display representation
{
    match output_type {
        OutputType::HumanReadable => {
            let mut table = prettytable::Table::new();
            table.add_row(Row::from(column_names));

            if let Some(values) = value.value().as_array() {
                for v in values {
                    let mut row_content = Vec::new();
                    for field in &field_names {
                        let display_value = if let Some(transformer) = &value_transformer {
                            transformer(field, &v[field])
                        } else {
                            Cell::new(v[field].as_str().unwrap_or("N/A"))
                        };
                        row_content.push(display_value);
                    }
                    table.add_row(Row::new(row_content));
                }
            }
            table.to_string()
        }
        OutputType::Json => serde_json::to_string_pretty(&value.value()).unwrap(),
    }
}

#[derive(Error, Debug)]
pub enum CommandError {
    #[error("auth error")]
    Authentication(#[from] AuthenticationError),
    #[error("request error: {0}")]
    Request(#[from] reqwest::Error),
    #[error("parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("parse error: {0}")]
    YamlParse(#[from] serde_yaml::Error),
    #[error("unknown error: {0}")]
    WrongResponseStatus(u16),
    #[error("Required field is missing in the response")]
    MissingField,
    #[error("Required file is missing in the edge app directory: {0}")]
    MissingRequiredFile(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid header value: {0}")]
    InvalidHeaderValue(#[from] InvalidHeaderValue),
    #[error("Cannot upload a new version: {0}")]
    NoChangesToUpload(String),
    #[error("Strip prefix error: {0}")]
    StripPrefixError(#[from] std::path::StripPrefixError),
    #[error("Filesystem error: {0}")]
    FileSystemError(String),
    #[error("Asset processing timeout")]
    AssetProcessingTimeout,
}

pub fn get(
    authentication: &Authentication,
    endpoint: &str,
) -> Result<serde_json::Value, CommandError> {
    let url = format!("{}/{}", &authentication.config.url, endpoint);
    let mut headers = HeaderMap::new();
    headers.insert("Prefer", "return=representation".parse()?);

    let response = authentication
        .build_client()?
        .get(url)
        .headers(headers)
        .send()?;

    let status = response.status();

    if status != StatusCode::OK {
        println!("Response: {:?}", &response.text());
        return Err(CommandError::WrongResponseStatus(status.as_u16()));
    }
    Ok(serde_json::from_str(&response.text()?)?)
}

pub fn post<T: Serialize + ?Sized>(
    authentication: &Authentication,
    endpoint: &str,
    payload: &T,
) -> Result<serde_json::Value, CommandError> {
    let url = format!("{}/{}", &authentication.config.url, endpoint);
    let mut headers = HeaderMap::new();
    headers.insert("Prefer", "return=representation".parse()?);

    let response = authentication
        .build_client()?
        .post(url)
        .headers(headers)
        .json(&payload)
        .send()?;

    let status = response.status();

    // Ok, No_Content are acceptable because some of our RPC code returns that.
    if ![StatusCode::CREATED, StatusCode::OK, StatusCode::NO_CONTENT].contains(&status) {
        debug!("Response: {:?}", &response.text()?);
        return Err(CommandError::WrongResponseStatus(status.as_u16()));
    }
    if status == StatusCode::NO_CONTENT {
        return Ok(serde_json::Value::Null);
    }

    Ok(serde_json::from_str(&response.text()?)?)
}

pub fn delete(authentication: &Authentication, endpoint: &str) -> anyhow::Result<(), CommandError> {
    let url = format!("{}/{}", &authentication.config.url, endpoint);
    let response = authentication.build_client()?.delete(url).send()?;
    if ![StatusCode::OK, StatusCode::NO_CONTENT].contains(&response.status()) {
        return Err(CommandError::WrongResponseStatus(
            response.status().as_u16(),
        ));
    }
    Ok(())
}

pub fn patch<T: Serialize + ?Sized>(
    authentication: &Authentication,
    endpoint: &str,
    payload: &T,
) -> anyhow::Result<serde_json::Value, CommandError> {
    let url = format!("{}/{}", &authentication.config.url, endpoint);
    let mut headers = HeaderMap::new();
    headers.insert("Prefer", "return=representation".parse()?);

    let response = authentication
        .build_client()?
        .patch(url)
        .json(&payload)
        .headers(headers)
        .send()?;

    let status = response.status();
    if status != StatusCode::OK {
        debug!("Response: {:?}", &response.text()?);
        return Err(CommandError::WrongResponseStatus(status.as_u16()));
    }

    if status == StatusCode::NO_CONTENT {
        return Ok(serde_json::Value::Null);
    }

    match serde_json::from_str(&response.text()?) {
        Ok(v) => Ok(v),
        Err(_) => Ok(serde_json::Value::Null),
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EdgeAppManifest {
    pub app_id: String,
    pub user_version: String,
    pub description: String,
    pub icon: String,
    pub author: String,
    pub homepage_url: String,
    #[serde(
        serialize_with = "serialize_settings",
        deserialize_with = "deserialize_settings",
        default
    )]
    pub settings: Vec<Setting>,
}

// maybe we can use a better name as we have EdgeAppSettings which is the same but serde_json::Value inside
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Setting {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default)]
    pub default_value: String,
    #[serde(default)]
    pub title: String,
    pub optional: bool,
    pub help_text: String,
}

fn deserialize_settings<'de, D>(deserializer: D) -> Result<Vec<Setting>, D::Error>
where
    D: Deserializer<'de>,
{
    let map: HashMap<String, Setting> = serde::Deserialize::deserialize(deserializer)?;
    let mut settings: Vec<Setting> = map
        .into_iter()
        .map(|(title, mut setting)| {
            setting.title = title;
            setting
        })
        .collect();
    settings.sort_by_key(|s| s.title.clone());
    Ok(settings)
}

fn serialize_settings<S>(settings: &[Setting], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;

    let mut map = serializer.serialize_map(Some(settings.len()))?;
    for setting in settings {
        map.serialize_entry(&setting.title, &setting)?;
    }
    map.end()
}

impl EdgeAppManifest {
    pub fn new(path: &Path) -> Result<Self, CommandError> {
        let data = fs::read_to_string(path)?;
        let manifest: EdgeAppManifest = serde_yaml::from_str(&data)?;
        Ok(manifest)
    }

    pub fn save_to_file(manifest: &EdgeAppManifest, path: &Path) -> Result<(), CommandError> {
        let yaml = serde_yaml::to_string(&manifest)?;
        let manifest_file = File::create(path)?;
        write!(&manifest_file, "---\n{yaml}")?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlaylistItem {
    pub asset_id: String,
    #[serde(deserialize_with = "deserialize_float_to_u32")]
    pub duration: u32,
    #[serde(skip_serializing, default = "default_pos_value")]
    pub position: u64,
}

fn default_pos_value() -> u64 {
    0
}

fn deserialize_float_to_u32<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let float_value: f64 = Deserialize::deserialize(deserializer)?;
    Ok(float_value as u32)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlaylistFile {
    predicate: String,
    playlist_id: String,
    items: Vec<PlaylistItem>,
}

impl PlaylistFile {
    pub fn new(
        predicate: String,
        playlist_id: String,
        items: serde_json::Value,
    ) -> Result<Self, CommandError> {
        Ok(Self {
            predicate,
            playlist_id,
            items: serde_json::from_value(items)?,
        })
    }
}

#[derive(Debug)]
pub struct EdgeApps {
    pub value: serde_json::Value,
}

impl EdgeApps {
    pub fn new(value: serde_json::Value) -> Self {
        Self { value }
    }
}
impl FormatterValue for EdgeApps {
    fn value(&self) -> &serde_json::Value {
        &self.value
    }
}

impl Formatter for EdgeApps {
    fn format(&self, output_type: OutputType) -> String {
        format_value(
            output_type,
            vec!["Id", "Title"],
            vec!["id", "name"],
            self,
            None::<fn(&str, &serde_json::Value) -> Cell>,
        )
    }
}

#[derive(Debug)]
pub struct EdgeAppVersions {
    pub value: serde_json::Value,
}

impl EdgeAppVersions {
    pub fn new(value: serde_json::Value) -> Self {
        Self { value }
    }
}

impl FormatterValue for EdgeAppVersions {
    fn value(&self) -> &serde_json::Value {
        &self.value
    }
}
impl Formatter for EdgeAppVersions {
    fn format(&self, output_type: OutputType) -> String {
        format_value(
            output_type,
            vec!["Revision", "Description", "Published"],
            vec!["revision", "description", "published"],
            self,
            Some(|field_name: &str, field_value: &serde_json::Value| {
                if field_name.eq("revision") {
                    let version = field_value.as_u64().unwrap_or(0);
                    let str_version = version.to_string();
                    Cell::new(if version > 0 { &str_version } else { "N/A" })
                } else if field_name.eq("published") {
                    let published = field_value.as_bool().unwrap_or(false);
                    Cell::new(if published { "✅" } else { "❌" })
                } else {
                    Cell::new(field_value.as_str().unwrap_or("N/A"))
                }
            }),
        )
    }
}
#[derive(Debug)]
pub struct EdgeAppSettings {
    pub value: serde_json::Value,
}

impl EdgeAppSettings {
    pub fn new(value: serde_json::Value) -> Self {
        Self { value }
    }
}

impl FormatterValue for EdgeAppSettings {
    fn value(&self) -> &serde_json::Value {
        &self.value
    }
}

impl Formatter for EdgeAppSettings {
    fn format(&self, output_type: OutputType) -> String {
        format_value(
            output_type,
            vec![
                "Title",
                "Value",
                "Default value",
                "Optional",
                "Type",
                "Help text",
            ],
            vec![
                "title",
                "value",
                "default_value",
                "optional",
                "type",
                "help_text",
            ],
            self,
            Some(
                |field_name: &str, field_value: &serde_json::Value| -> Cell {
                    if field_name.eq("optional") {
                        let value = field_value.as_bool().unwrap_or(false);
                        return Cell::new(if value { "Yes" } else { "No" });
                    }
                    Cell::new(field_value.as_str().unwrap_or_default())
                },
            ),
        )
    }
}

#[derive(Debug)]
pub struct Assets {
    pub value: serde_json::Value,
}

impl Assets {
    pub fn new(value: serde_json::Value) -> Self {
        Self { value }
    }
}

impl FormatterValue for Assets {
    fn value(&self) -> &serde_json::Value {
        &self.value
    }
}

impl Formatter for Assets {
    fn format(&self, output_type: OutputType) -> String {
        format_value(
            output_type,
            vec!["Id", "Title", "Type", "Status"],
            vec!["id", "title", "type", "status"],
            self,
            None::<fn(&str, &serde_json::Value) -> Cell>,
        )
    }
}

#[derive(Debug)]
pub struct Screens {
    pub value: serde_json::Value,
}

impl Screens {
    pub fn new(value: serde_json::Value) -> Self {
        Self { value }
    }
}

impl FormatterValue for Screens {
    fn value(&self) -> &serde_json::Value {
        &self.value
    }
}

impl Formatter for Screens {
    fn format(&self, output_type: OutputType) -> String {
        format_value(
            output_type,
            vec![
                "Id",
                "Name",
                "Hardware Version",
                "In Sync",
                "Last Ping",
                "Uptime",
            ],
            vec![
                "id",
                "name",
                "hardware_version",
                "in_sync",
                "last_ping",
                "uptime",
            ],
            self,
            Some(|field: &str, value: &serde_json::Value| {
                if field.eq("in_sync") {
                    if value.as_bool().unwrap_or(false) {
                        cell!(c -> "✅")
                    } else {
                        cell!(c -> "❌")
                    }
                } else if field.eq("uptime") {
                    let uptime = if let Some(uptime) = value.as_u64() {
                        indicatif::HumanDuration(Duration::new(uptime, 0)).to_string()
                    } else {
                        "N/A".to_owned()
                    };
                    Cell::new(&uptime).style_spec("r")
                } else {
                    Cell::new(value.as_str().unwrap_or("N/A"))
                }
            }),
        )
    }
}

#[derive(Debug)]
pub struct Playlists {
    pub value: serde_json::Value,
}

impl Playlists {
    pub fn new(value: serde_json::Value) -> Self {
        Self { value }
    }
}

impl FormatterValue for Playlists {
    fn value(&self) -> &serde_json::Value {
        &self.value
    }
}

impl Formatter for Playlists {
    fn format(&self, output_type: OutputType) -> String {
        format_value(
            output_type,
            vec!["Id", "Title"],
            vec!["id", "title"],
            self,
            None::<fn(&str, &serde_json::Value) -> Cell>,
        )
    }
}

#[derive(Debug)]
pub struct PlaylistItems {
    pub value: serde_json::Value,
}

impl PlaylistItems {
    pub fn new(value: serde_json::Value) -> Self {
        Self { value }
    }
}

impl FormatterValue for PlaylistItems {
    fn value(&self) -> &serde_json::Value {
        &self.value
    }
}

impl Formatter for PlaylistItems {
    fn format(&self, output_type: OutputType) -> String {
        format_value(
            output_type,
            vec!["Asset Id", "Duration"],
            vec!["asset_id", "duration"],
            self,
            Some(|field: &str, value: &serde_json::Value| {
                if field.eq("duration") {
                    cell!(indicatif::HumanDuration(Duration::from_secs(
                        value.as_f64().unwrap_or(0.0) as u64
                    ))
                    .to_string())
                } else {
                    Cell::new(value.as_str().unwrap_or("N/A"))
                }
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use std::io::Read;

    #[test]
    fn test_save_to_file_should_save_yaml_correctly() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.yaml");

        let manifest = EdgeAppManifest {
            app_id: "test_app".to_string(),
            settings: vec![Setting {
                title: "username".to_string(),
                type_: "string".to_string(),
                default_value: "stranger".to_string(),
                optional: true,
                help_text: "An example of a setting that is used in index.html".to_string(),
            }],
            ..Default::default()
        };

        EdgeAppManifest::save_to_file(&manifest, &file_path).unwrap();

        let mut file = File::open(&file_path).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();

        let expected_yaml = serde_yaml::to_string(&manifest).unwrap();
        let expected_contents = format!("---\n{}", expected_yaml);

        assert_eq!(contents, expected_contents);

        dir.close().unwrap();
    }
}