use rusqlite::{params, Connection, OpenFlags};
use std::{net::IpAddr, path::Path};
use thiserror::Error;

pub fn load_client_key(path: &Path, tv_ip: IpAddr) -> Result<String, KeyStoreError> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(KeyStoreError::Sqlite)?;
    let value: Vec<u8> = connection
        .query_row(
            "SELECT value FROM unnamed WHERE key = ?1",
            params![tv_ip.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => KeyStoreError::MissingKey(tv_ip),
            other => KeyStoreError::Sqlite(other),
        })?;
    decode_pickled_string(&value)
}

fn decode_pickled_string(value: &[u8]) -> Result<String, KeyStoreError> {
    let (offset, length) = value
        .iter()
        .enumerate()
        .find_map(|(index, opcode)| match opcode {
            0x8c => value
                .get(index + 1)
                .map(|length| (index + 2, *length as usize)),
            0x58 => value.get(index + 1..index + 5).map(|length| {
                (
                    index + 5,
                    u32::from_le_bytes(length.try_into().expect("four bytes")) as usize,
                )
            }),
            0x8d => value.get(index + 1..index + 9).and_then(|length| {
                usize::try_from(u64::from_le_bytes(length.try_into().expect("eight bytes")))
                    .ok()
                    .map(|length| (index + 9, length))
            }),
            _ => None,
        })
        .ok_or(KeyStoreError::UnsupportedPickle)?;
    let end = offset
        .checked_add(length)
        .filter(|end| *end <= value.len())
        .ok_or(KeyStoreError::InvalidPickleLength)?;
    let key = std::str::from_utf8(&value[offset..end])
        .map_err(KeyStoreError::Utf8)?
        .to_string();
    if key.is_empty() {
        return Err(KeyStoreError::EmptyKey);
    }
    Ok(key)
}

#[derive(Debug, Error)]
pub enum KeyStoreError {
    #[error("client-key database error: {0}")]
    Sqlite(rusqlite::Error),
    #[error("no client key stored for TV {0}")]
    MissingKey(IpAddr),
    #[error("client-key value uses an unsupported pickle representation")]
    UnsupportedPickle,
    #[error("client-key pickle length is invalid")]
    InvalidPickleLength,
    #[error("client-key is not UTF-8: {0}")]
    Utf8(std::str::Utf8Error),
    #[error("client-key is empty")]
    EmptyKey,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_sqlitedict_short_unicode_pickle() {
        let mut value = vec![0x80, 0x05, 0x8c, 0x08];
        value.extend_from_slice(b"key-1234");
        value.extend_from_slice(&[0x94, 0x2e]);
        assert_eq!(decode_pickled_string(&value).unwrap(), "key-1234");
    }

    #[test]
    fn rejects_non_string_pickle() {
        assert!(matches!(
            decode_pickled_string(&[0x80, 0x05, 0x4b, 0x01, 0x2e]),
            Err(KeyStoreError::UnsupportedPickle)
        ));
    }
}
