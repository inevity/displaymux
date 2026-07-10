use crate::domain::{Host, TvMode};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use thiserror::Error;

pub const GET_CURRENT_APP: &str = "com.webos.applicationManager/getForegroundAppInfo";
pub const GET_INPUTS: &str = "tv/getExternalInputList";
pub const SET_INPUT: &str = "tv/switchInput";
pub const GET_SYSTEM_SETTINGS: &str = "settings/getSystemSettings";
pub const SET_SYSTEM_SETTINGS: &str = "settings/setSystemSettings";
pub const MULTIVIEW_SUBSCRIPTION_ID: &str = "subscribe-multiview";

const SIGNATURE: &str = concat!(
    "eyJhbGdvcml0aG0iOiJSU0EtU0hBMjU2Iiwia2V5SWQiOiJ0ZXN0LXNpZ25pbm",
    "ctY2VydCIsInNpZ25hdHVyZVZlcnNpb24iOjF9.hrVRgjCwXVvE2OOSpDZ58hR",
    "+59aFNwYDyjQgKk3auukd7pcegmE2CzPCa0bJ0ZsRAcKkCTJrWo5iDzNhMBWRy",
    "aMOv5zWSrthlf7G128qvIlpMT0YNY+n/FaOHE73uLrS/g7swl3/qH/BGFG2Hu4",
    "RlL48eb3lLKqTt2xKHdCs6Cd4RMfJPYnzgvI4BNrFUKsjkcu+WD4OO2A27Pq1n",
    "50cMchmcaXadJhGrOqH5YmHdOCj5NSHzJYrsW0HPlpuAx/ECMeIZYDh6RMqaFM",
    "2DXzdKX9NmmyqzJ3o/0lkk/N97gfVRLW5hA29yeAwaCViZNCP8iC9aO0q9fQoj",
    "oa7NQnAtw=="
);

pub fn registration(client_key: &str) -> Value {
    json!({
        "type": "register",
        "id": "register-0",
        "payload": {
            "client-key": client_key,
            "forcePairing": false,
            "pairingType": "PROMPT",
            "manifest": manifest(),
        }
    })
}

pub fn request(id: &str, uri: &str, payload: Value) -> Value {
    json!({
        "id": id,
        "type": "request",
        "uri": format!("ssap://{uri}"),
        "payload": payload,
    })
}

pub fn subscription() -> Value {
    json!({
        "id": MULTIVIEW_SUBSCRIPTION_ID,
        "type": "subscribe",
        "uri": format!("ssap://{GET_SYSTEM_SETTINGS}"),
        "payload": {
            "category": "option",
            "keys": ["multiViewStatus"],
        }
    })
}

pub fn response_id(message: &Value) -> Option<&str> {
    message.get("id")?.as_str()
}

pub fn successful_payload(message: &Value) -> Result<&Value, CodecError> {
    if message.get("type").and_then(Value::as_str) == Some("error") {
        return Err(CodecError::Command(
            message
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown SSAP error")
                .to_string(),
        ));
    }
    let payload = message.get("payload").ok_or(CodecError::MissingPayload)?;
    let success = payload
        .get("returnValue")
        .and_then(Value::as_bool)
        .or_else(|| payload.get("subscribed").and_then(Value::as_bool));
    if success == Some(false) {
        return Err(CodecError::Command(message.to_string()));
    }
    Ok(payload)
}

pub fn registered_client_key(message: &Value) -> Result<Option<&str>, CodecError> {
    match message.get("type").and_then(Value::as_str) {
        Some("registered") => Ok(message
            .get("payload")
            .and_then(|payload| payload.get("client-key"))
            .and_then(Value::as_str)),
        Some("response") => Ok(None),
        Some("error") => Err(CodecError::Command(message.to_string())),
        _ => Err(CodecError::UnexpectedRegistration(message.to_string())),
    }
}

pub fn parse_multiview_mode(payload: &Value) -> Option<TvMode> {
    match payload
        .get("settings")
        .and_then(|settings| settings.get("multiViewStatus"))
        .and_then(Value::as_str)
    {
        Some("on") => Some(TvMode::Multiview),
        Some("off") => Some(TvMode::Fullscreen),
        _ => None,
    }
}

pub fn parse_current_input(
    payload: &Value,
    inputs: &BTreeMap<Host, String>,
) -> Result<Option<Host>, CodecError> {
    let app_id = payload
        .get("appId")
        .and_then(Value::as_str)
        .ok_or(CodecError::MissingField("appId"))?;
    Ok(inputs.iter().find_map(|(host, input_id)| {
        let expected_app = format!(
            "com.webos.app.{}",
            input_id.to_ascii_lowercase().replace('_', "")
        );
        (app_id.eq_ignore_ascii_case(input_id) || app_id.eq_ignore_ascii_case(&expected_app))
            .then_some(*host)
    }))
}

pub fn parse_signals(
    payload: &Value,
    inputs: &BTreeMap<Host, String>,
) -> Result<BTreeMap<Host, bool>, CodecError> {
    let devices = payload
        .get("devices")
        .and_then(Value::as_array)
        .ok_or(CodecError::MissingField("devices"))?;
    Ok(inputs
        .iter()
        .map(|(host, input_id)| {
            let present = devices.iter().find_map(|device| {
                let id = device.get("id").and_then(Value::as_str)?;
                id.eq_ignore_ascii_case(input_id).then(|| {
                    device
                        .get("hdmiSignalExist")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
            });
            (*host, present.unwrap_or(false))
        })
        .collect())
}

fn manifest() -> Value {
    json!({
        "appVersion": "1.1",
        "manifestVersion": 1,
        "permissions": [
            "LAUNCH", "LAUNCH_WEBAPP", "APP_TO_APP", "CLOSE", "TEST_OPEN",
            "TEST_PROTECTED", "CONTROL_AUDIO", "CONTROL_DISPLAY",
            "CONTROL_INPUT_JOYSTICK", "CONTROL_INPUT_MEDIA_RECORDING",
            "CONTROL_INPUT_MEDIA_PLAYBACK", "CONTROL_INPUT_TV", "CONTROL_POWER",
            "CONTROL_TV_SCREEN", "READ_APP_STATUS", "READ_CURRENT_CHANNEL",
            "READ_INPUT_DEVICE_LIST", "READ_NETWORK_STATE", "READ_RUNNING_APPS",
            "READ_TV_CHANNEL_LIST", "WRITE_NOTIFICATION_TOAST", "READ_POWER_STATE",
            "READ_COUNTRY_INFO", "CONTROL_INPUT_TEXT", "CONTROL_MOUSE_AND_KEYBOARD",
            "READ_INSTALLED_APPS", "READ_SETTINGS", "READ_STORAGE_DEVICE_LIST"
        ],
        "signatures": [{"signature": SIGNATURE, "signatureVersion": 1}],
        "signed": {
            "appId": "com.lge.test",
            "created": "20140509",
            "localizedAppNames": {"": "LG Remote App"},
            "localizedVendorNames": {"": "LG Electronics"},
            "permissions": [
                "TEST_SECURE", "CONTROL_INPUT_TEXT", "CONTROL_MOUSE_AND_KEYBOARD",
                "READ_INSTALLED_APPS", "READ_LGE_SDX", "READ_NOTIFICATIONS", "SEARCH",
                "WRITE_SETTINGS", "WRITE_NOTIFICATION_ALERT", "CONTROL_POWER",
                "READ_CURRENT_CHANNEL", "READ_RUNNING_APPS", "READ_UPDATE_INFO",
                "UPDATE_FROM_REMOTE_APP", "READ_LGE_TV_INPUT_EVENTS", "READ_TV_CURRENT_TIME"
            ],
            "serial": "2f930e2d2cfe083771f68e4fe7bb07",
            "vendorId": "com.lge"
        }
    })
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CodecError {
    #[error("SSAP response has no payload")]
    MissingPayload,
    #[error("SSAP payload is missing {0}")]
    MissingField(&'static str),
    #[error("SSAP command failed: {0}")]
    Command(String),
    #[error("unexpected registration response: {0}")]
    UnexpectedRegistration(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> BTreeMap<Host, String> {
        BTreeMap::from([
            (Host::Linux, "HDMI_4".to_string()),
            (Host::Mac, "HDMI_3".to_string()),
            (Host::Windows, "HDMI_2".to_string()),
        ])
    }

    #[test]
    fn parses_live_g4_input_and_signal_fields() {
        assert_eq!(
            parse_current_input(&json!({"appId": "com.webos.app.hdmi4"}), &inputs()).unwrap(),
            Some(Host::Linux)
        );
        let signals = parse_signals(
            &json!({
                "devices": [
                    {"id": "HDMI_2", "hdmiSignalExist": false},
                    {"id": "HDMI_3", "hdmiSignalExist": false},
                    {"id": "HDMI_4", "hdmiSignalExist": true}
                ]
            }),
            &inputs(),
        )
        .unwrap();
        assert_eq!(signals.get(&Host::Linux), Some(&true));
        assert_eq!(signals.get(&Host::Windows), Some(&false));
    }

    #[test]
    fn parses_multiview_subscription() {
        assert_eq!(
            parse_multiview_mode(&json!({"settings": {"multiViewStatus": "on"}})),
            Some(TvMode::Multiview)
        );
    }

    #[test]
    fn registration_contains_existing_client_key_without_logging_it() {
        let message = registration("secret-key");
        assert_eq!(message["payload"]["client-key"], "secret-key");
        assert_eq!(message["type"], "register");
    }
}
